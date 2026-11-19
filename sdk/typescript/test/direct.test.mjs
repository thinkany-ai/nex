// Direct paths: a forward reaches a host's tcp service without the relay once
// the host offers direct paths, keeps working while the relay is down, falls
// back to the relay when the direct path breaks, and refuses devices without
// a key.

import assert from "node:assert/strict";
import tls from "node:tls";
import { after, before, test } from "node:test";
import {
  DIRECT_SERVICE,
  DirectPaths,
  NexClient,
  NexHost,
  classifyAddress,
  fetchDirectInfo,
  forwardTcp,
  tcpService,
} from "../dist/node/index.js";
import { runHost, startRelay, tcpClientRoundtrip, tcpEcho, waitFor, waitOnline } from "./helpers.mjs";

let relay;
const cleanups = [];
const s = {};

before(async () => {
  relay = await startRelay();
  const hostCred = await NexClient.bootstrap(relay.url, { accountName: "direct", deviceName: "mini" });
  s.host = new NexClient({ server: relay.url, credential: hostCred });
  const code = await s.host.createPairingCode();
  s.laptop = new NexClient({
    server: relay.url,
    credential: await NexClient.claim(relay.url, { code: code.code, deviceName: "laptop" }),
  });
  s.shell = await tcpEcho("mini:");
  cleanups.push(s.shell.close);
});

after(async () => {
  for (const fn of cleanups.reverse()) await fn().catch(() => {});
  await relay?.stop();
});

function startHost(options = {}) {
  const host = new NexHost({
    client: s.host,
    services: [tcpService("shell", s.shell.addr)],
    direct: { addresses: ["127.0.0.1"] },
    ...options,
  });
  const run = runHost(host);
  cleanups.push(run.stop);
  return { host, run };
}

test("classifies tailnet and LAN addresses; never offers public ones", () => {
  assert.equal(classifyAddress("100.109.224.114"), "tailscale");
  assert.equal(classifyAddress("100.63.0.1"), null);
  assert.equal(classifyAddress("fd7a:115c:a1e0::1"), "tailscale");
  assert.equal(classifyAddress("192.168.1.20"), "lan");
  assert.equal(classifyAddress("10.0.0.2"), "lan");
  assert.equal(classifyAddress("172.20.1.1"), "lan");
  assert.equal(classifyAddress("172.32.1.1"), null);
  assert.equal(classifyAddress("8.8.8.8"), null);
  assert.equal(classifyAddress("fe80::1"), null);
});

test("host advertises nex.direct and a per-device key", async () => {
  s.first = startHost();
  await waitOnline(s.laptop, s.host.deviceId);
  await waitFor(() => s.first.host.directEndpoints.length === 1, { what: "direct listener" });
  const [mini] = (await s.laptop.devices()).filter((d) => d.id === s.host.deviceId);
  assert.ok(mini.services.some((x) => x.name === DIRECT_SERVICE && x.kind === "ws"));

  const info = await fetchDirectInfo(s.laptop, "mini");
  assert.equal(info.v, 1);
  assert.deepEqual(info.services, ["shell"]);
  assert.deepEqual(info.endpoints.map((e) => [e.host, e.kind]), [["127.0.0.1", "custom"]]);
  assert.equal(Buffer.from(info.key, "base64").length, 32);
  s.info = info;
});

test("forwardTcp goes direct and survives a relay outage", async () => {
  // Re-discover often, so the outage below makes discovery fail repeatedly.
  const paths = new DirectPaths(s.laptop, "mini", { refreshIntervalMs: 100, probeIntervalMs: 100 }).start();
  cleanups.push(async () => paths.stop());
  const fwd = await forwardTcp(s.laptop, "mini", "shell", { direct: paths });
  cleanups.push(() => new Promise((r) => fwd.close(() => r())));
  const port = fwd.address().port;
  assert.equal(await tcpClientRoundtrip(port, "hi", 7), "mini:hi");
  const status = fwd.paths.status();
  assert.equal(status.route, "direct", JSON.stringify(status));
  assert.equal(status.endpoint.host, "127.0.0.1");
  assert.ok(status.relaySetupMs > 0);

  // With the relay gone, new connections still reach the host directly.
  await relay.stop();
  await new Promise((r) => setTimeout(r, 500)); // several failed discoveries and revalidations
  assert.equal(paths.status().route, "direct");
  const many = await Promise.all(Array.from({ length: 5 }, (_, i) => tcpClientRoundtrip(port, `m${i}`, 7)));
  assert.deepEqual(many, ["mini:m0", "mini:m1", "mini:m2", "mini:m3", "mini:m4"]);
});

test("a device without the key, or with a stale one, is refused", async () => {
  const [ep] = s.info.endpoints;
  const attempt = (identity, psk) =>
    new Promise((resolve) => {
      const sock = tls.connect({
        host: ep.host,
        port: ep.port,
        minVersion: "TLSv1.2",
        maxVersion: "TLSv1.2",
        ciphers: "ECDHE-PSK-CHACHA20-POLY1305",
        pskCallback: () => ({ psk, identity }),
        checkServerIdentity: () => undefined,
      });
      sock.once("secureConnect", () => {
        sock.write('{"probe":true}\n');
        sock.once("data", (d) => (sock.destroy(), resolve(d.toString())));
      });
      sock.once("error", (e) => resolve(`error: ${e.message}`));
    });
  const key = Buffer.from(s.info.key, "base64");
  assert.equal(await attempt(`nex1:${s.laptop.deviceId}`, key), '{"ok":true}\n');
  // Right key, different identity: a device that never asked the relay.
  assert.match(await attempt(`nex1:dev_somebody`, key), /^error:.*psk/i);
  // Right identity, wrong key.
  assert.match(await attempt(`nex1:${s.laptop.deviceId}`, Buffer.alloc(32, 1)), /^error:/);
});

test("falls back to the relay when the direct path breaks, then recovers", async () => {
  relay = await startRelay();
  // Fresh relay, fresh accounts.
  const hostCred = await NexClient.bootstrap(relay.url, { accountName: "direct2", deviceName: "mini" });
  s.host = new NexClient({ server: relay.url, credential: hostCred });
  const code = await s.host.createPairingCode();
  s.laptop = new NexClient({
    server: relay.url,
    credential: await NexClient.claim(relay.url, { code: code.code, deviceName: "laptop" }),
  });
  await s.first.run.stop();

  let a = startHost();
  await waitOnline(s.laptop, s.host.deviceId);
  const paths = new DirectPaths(s.laptop, "mini", { probeIntervalMs: 200 }).start();
  cleanups.push(async () => paths.stop());
  const fwd = await forwardTcp(s.laptop, "mini", "shell", { direct: paths });
  cleanups.push(() => new Promise((r) => fwd.close(() => r())));
  const port = fwd.address().port;
  assert.equal(await tcpClientRoundtrip(port, "a", 6), "mini:a");
  assert.equal(paths.status().route, "direct");

  // Restart the host: its old listener and keys are gone. The next connection
  // must still work (relay), and direct comes back with the new key.
  await a.run.stop();
  a = startHost();
  await waitOnline(s.laptop, s.host.deviceId);
  assert.equal(await tcpClientRoundtrip(port, "b", 6), "mini:b");
  await waitFor(() => paths.status().route === "direct", { timeoutMs: 8000, what: "direct path again" });
  assert.equal(await tcpClientRoundtrip(port, "c", 6), "mini:c");
});

test("hosts without direct paths: relay only, and the reason is reported", async () => {
  await Promise.all(cleanups.splice(0).map((fn) => fn().catch(() => {})));
  s.shell = await tcpEcho("mini:");
  cleanups.push(s.shell.close);
  startHost({ direct: undefined });
  await waitOnline(s.laptop, s.host.deviceId);
  const fwd = await forwardTcp(s.laptop, "mini", "shell");
  cleanups.push(() => new Promise((r) => fwd.close(() => r())));
  assert.equal(await tcpClientRoundtrip(fwd.address().port, "r", 6), "mini:r");
  const status = fwd.paths.status();
  assert.equal(status.route, "relay");
  assert.match(status.error, /does not offer direct paths/);
});
