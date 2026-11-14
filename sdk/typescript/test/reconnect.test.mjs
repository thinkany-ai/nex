// The host reconnects (1 s first backoff) when its relay connection drops for
// a non-fatal reason. A TCP proxy in front of the relay cuts the connection.

import assert from "node:assert/strict";
import net from "node:net";
import { after, before, test } from "node:test";
import { NexClient, NexHost, echoService } from "../dist/node/index.js";
import { runHost, startRelay, waitFor, waitOnline, wsRoundtrip } from "./helpers.mjs";

let relay;
let proxy;
const live = new Set();
const cleanups = [];

before(async () => {
  relay = await startRelay();
  const target = new URL(relay.url);
  proxy = net.createServer((client) => {
    const upstream = net.connect(Number(target.port), target.hostname);
    live.add(client);
    live.add(upstream);
    const drop = () => {
      client.destroy();
      upstream.destroy();
      live.delete(client);
      live.delete(upstream);
    };
    client.on("error", drop).on("close", drop);
    upstream.on("error", drop).on("close", drop);
    client.pipe(upstream).pipe(client);
  });
  await new Promise((r) => proxy.listen(0, "127.0.0.1", r));
});

after(async () => {
  for (const fn of cleanups.reverse()) await fn().catch(() => {});
  for (const s of live) s.destroy();
  await new Promise((r) => proxy.close(r));
  await relay?.stop();
});

test("host comes back online after the connection drops", async () => {
  const cred = await NexClient.bootstrap(relay.url, { accountName: "r", deviceName: "host" });
  const viaProxy = new NexClient({ server: `http://127.0.0.1:${proxy.address().port}/`, credential: cred });
  const code = await viaProxy.createPairingCode();
  const observer = new NexClient({ server: relay.url, credential: await NexClient.claim(relay.url, { code: code.code, deviceName: "obs" }) });

  const events = [];
  const host = new NexHost({
    client: viaProxy,
    services: [echoService()],
    onOnline: () => events.push("online"),
    onOffline: (e) => events.push(`offline:${e ? "error" : "null"}`),
  });
  const h = runHost(host);
  cleanups.push(h.stop);
  await waitOnline(observer, cred.device_id);

  for (const s of live) s.destroy(); // cut the control connection
  await waitFor(() => events.includes("offline:error"), { what: "offline event" });
  await waitFor(() => events.filter((e) => e === "online").length === 2, { timeoutMs: 5000, what: "reconnect" });
  await waitOnline(observer, cred.device_id);
  assert.equal(await wsRoundtrip(observer, "host", "echo", "back"), "back");

  await h.stop();
  assert.deepEqual(events, ["online", "offline:error", "online", "offline:null"]);
});
