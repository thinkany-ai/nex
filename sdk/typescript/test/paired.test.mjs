// Paired devices: keys exchanged by the user, never by the relay.
// - with no relay and no account at all (PeerHost + forwardPeer)
// - through the relay, end-to-end encrypted (nex.e2e), and `strict` hosts
//   refusing everyone else
// - never a silent downgrade to plaintext when the pairing does not match

import assert from "node:assert/strict";
import { randomBytes } from "node:crypto";
import net from "node:net";
import tls from "node:tls";
import { after, before, test } from "node:test";
import {
  NexClient,
  NexHost,
  PeerHost,
  fingerprint,
  forwardPeer,
  forwardTcp,
  generateIdentity,
  normalizePublicKey,
  pairedKey,
  pairingKey,
  sendPairingRequest,
  tcpService,
} from "../dist/node/index.js";
import { runHost, startRelay, tcpClientRoundtrip, tcpEcho, waitFor, waitOnline } from "./helpers.mjs";

const cleanups = [];
const closeServer = (server) => () => new Promise((r) => server.close(() => r()));
after(async () => {
  for (const fn of cleanups.reverse()) await fn().catch(() => {});
});

/** Resolves "closed" when the forward drops the connection without data. */
function refused(port) {
  return new Promise((resolve) => {
    const sock = net.connect({ host: "127.0.0.1", port }, () => sock.write("x"));
    sock.on("data", (d) => resolve(`data:${d}`));
    sock.on("error", () => resolve("closed"));
    sock.on("close", () => resolve("closed"));
  });
}

test("identities, fingerprints and the shared key", () => {
  const a = generateIdentity();
  const b = generateIdentity();
  assert.match(a.publicKey, /^nexpk1\.[A-Za-z0-9_-]{43}$/);
  assert.match(fingerprint(a.publicKey), /^[A-Z2-9]{4}(-[A-Z2-9]{4}){4}$/);
  assert.notEqual(fingerprint(a.publicKey), fingerprint(b.publicKey));
  // Both sides derive the same key; a third party cannot.
  assert.deepEqual(pairedKey(a, b.publicKey), pairedKey(b, a.publicKey));
  assert.notDeepEqual(pairedKey(a, b.publicKey), pairedKey(generateIdentity(), b.publicKey));
  assert.equal(normalizePublicKey(`  ${a.publicKey}\n`), a.publicKey);
  assert.throws(() => normalizePublicKey("nexpk1.short"));
  assert.throws(() => normalizePublicKey(a.privateKey));
});

test("no relay, no account: paired devices connect over the LAN", async () => {
  const shell = await tcpEcho("mini:");
  cleanups.push(shell.close);
  const mini = generateIdentity();
  const laptop = generateIdentity();
  const host = new PeerHost({
    identity: mini,
    peers: [laptop.publicKey],
    services: [tcpService("shell", shell.addr)],
    listen: { addresses: ["127.0.0.1"], port: 0 },
  });
  host.start();
  cleanups.push(async () => host.stop());
  await waitFor(() => host.endpoints.length === 1, { what: "peer listener" });
  const [ep] = host.endpoints;

  const fwd = await forwardPeer([ep], { identity: laptop, peer: mini.publicKey }, "shell");
  cleanups.push(closeServer(fwd));
  assert.equal(await tcpClientRoundtrip(fwd.address().port, "hi", 7), "mini:hi");
  assert.equal(fwd.paths.status().route, "direct");

  // A device the host never paired with gets nothing.
  const stranger = await forwardPeer([ep], { identity: generateIdentity(), peer: mini.publicKey }, "shell");
  cleanups.push(closeServer(stranger));
  assert.equal(await refused(stranger.address().port), "closed");

  // Dialing an impostor (wrong host key pinned) fails too.
  const wrongHost = await forwardPeer([ep], { identity: laptop, peer: generateIdentity().publicKey }, "shell");
  cleanups.push(closeServer(wrongHost));
  assert.equal(await refused(wrongHost.address().port), "closed");

  // Unpairing takes effect for new connections.
  host.updatePeers([]);
  const fwd2 = await forwardPeer([ep], { identity: laptop, peer: mini.publicKey }, "shell");
  cleanups.push(closeServer(fwd2));
  assert.equal(await refused(fwd2.address().port), "closed");
});

test("through the relay: end-to-end encrypted, strict hosts refuse plaintext", async () => {
  const relay = await startRelay();
  cleanups.push(() => relay.stop());
  const shell = await tcpEcho("mini:");
  cleanups.push(shell.close);
  const miniClient = new NexClient({
    server: relay.url,
    credential: await NexClient.bootstrap(relay.url, { accountName: "p", deviceName: "mini" }),
  });
  const code = await miniClient.createPairingCode();
  const laptopClient = new NexClient({
    server: relay.url,
    credential: await NexClient.claim(relay.url, { code: code.code, deviceName: "laptop" }),
  });
  const mini = generateIdentity();
  const laptop = generateIdentity();
  const host = new NexHost({
    client: miniClient,
    services: [tcpService("shell", shell.addr)],
    trust: { identity: mini, peers: [laptop.publicKey], strict: true },
  });
  const run = runHost(host);
  cleanups.push(run.stop);
  await waitOnline(laptopClient, miniClient.deviceId);

  // Paired: works, over nex.e2e (no direct paths offered here).
  const paired = await forwardTcp(laptopClient, "mini", "shell", { trust: { identity: laptop, peer: mini.publicKey } });
  cleanups.push(closeServer(paired));
  assert.equal(await tcpClientRoundtrip(paired.address().port, "e2e", 8), "mini:e2e");

  // Same account, not paired: the strict host refuses the plaintext stream.
  const plain = await forwardTcp(laptopClient, "mini", "shell");
  cleanups.push(closeServer(plain));
  assert.equal(await refused(plain.address().port), "closed");

  // Pinned the wrong key: fails, and never falls back to plaintext.
  const wrong = await forwardTcp(laptopClient, "mini", "shell", {
    trust: { identity: laptop, peer: generateIdentity().publicKey },
  });
  cleanups.push(closeServer(wrong));
  assert.equal(await refused(wrong.address().port), "closed");

  // Turning strict off lets the rest of the account back in.
  host.updateTrust({ identity: mini, peers: [laptop.publicKey], strict: false });
  assert.equal(await tcpClientRoundtrip(plain.address().port, "plain", 10), "mini:plain");
});

test("paired devices use the paired key on direct paths too", async () => {
  const relay = await startRelay();
  cleanups.push(() => relay.stop());
  const shell = await tcpEcho("mini:");
  cleanups.push(shell.close);
  const miniClient = new NexClient({
    server: relay.url,
    credential: await NexClient.bootstrap(relay.url, { accountName: "q", deviceName: "mini" }),
  });
  const code = await miniClient.createPairingCode();
  const laptopClient = new NexClient({
    server: relay.url,
    credential: await NexClient.claim(relay.url, { code: code.code, deviceName: "laptop" }),
  });
  const mini = generateIdentity();
  const laptop = generateIdentity();
  const host = new NexHost({
    client: miniClient,
    services: [tcpService("shell", shell.addr)],
    direct: { addresses: ["127.0.0.1"] },
    trust: { identity: mini, peers: [laptop.publicKey], strict: true },
  });
  const run = runHost(host);
  cleanups.push(run.stop);
  await waitOnline(laptopClient, miniClient.deviceId);

  const fwd = await forwardTcp(laptopClient, "mini", "shell", { trust: { identity: laptop, peer: mini.publicKey } });
  cleanups.push(closeServer(fwd));
  assert.equal(await tcpClientRoundtrip(fwd.address().port, "d", 6), "mini:d");
  assert.equal(fwd.paths.status().route, "direct");

  // Strict: the relay hands out no key, so an unpaired device has no direct path either.
  const plain = await forwardTcp(laptopClient, "mini", "shell");
  cleanups.push(closeServer(plain));
  assert.equal(await refused(plain.address().port), "closed");
  assert.match(plain.paths.status().error ?? "", /only accepts paired devices/);
});

test("pairing requests: whoever has the secret may knock, and do nothing else", async () => {
  const shell = await tcpEcho("mini:");
  cleanups.push(shell.close);
  const secret = randomBytes(16);
  const received = [];
  const host = new PeerHost({
    identity: generateIdentity(),
    peers: [],
    services: [tcpService("shell", shell.addr)],
    listen: { addresses: ["127.0.0.1"], port: 0 },
    pairing: {
      secret,
      onRequest: (payload) => {
        received.push(payload);
        return payload?.name === "spam" ? "too many requests" : null;
      },
    },
  });
  host.start();
  cleanups.push(async () => host.stop());
  await waitFor(() => host.endpoints.length === 1, { what: "peer listener" });
  const eps = [{ host: "127.0.0.1", port: 9 }, ...host.endpoints]; // one dead address is fine

  await sendPairingRequest(eps, secret, { name: "laptop", invite: "termany1.x" });
  assert.deepEqual(received, [{ name: "laptop", invite: "termany1.x" }]);
  await assert.rejects(sendPairingRequest(host.endpoints, secret, { name: "spam" }), /too many requests/);
  await assert.rejects(sendPairingRequest(host.endpoints, randomBytes(16), { name: "x" }));
  assert.equal(received.length, 2);

  // A pairing connection asking for a service instead is refused.
  const [ep] = host.endpoints;
  const reply = await new Promise((resolve) => {
    const sock = tls.connect({
      host: ep.host,
      port: ep.port,
      minVersion: "TLSv1.2",
      maxVersion: "TLSv1.2",
      ciphers: "ECDHE-PSK-CHACHA20-POLY1305",
      pskCallback: () => ({ psk: pairingKey(secret), identity: "nexpair:1" }),
      checkServerIdentity: () => undefined,
    });
    sock.once("secureConnect", () => sock.write('{"service":"shell"}\n'));
    sock.on("data", (d) => (sock.destroy(), resolve(d.toString())));
    sock.on("error", (e) => resolve(`error ${e.message}`));
  });
  assert.match(reply, /may only ask to pair/);

  // Closing pairing shuts the door.
  host.updatePairing(null);
  await assert.rejects(sendPairingRequest(host.endpoints, secret, { name: "late" }));
});
