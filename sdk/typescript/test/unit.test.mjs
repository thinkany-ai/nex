// Pure unit tests (no relay).

import assert from "node:assert/strict";
import { test } from "node:test";
import * as iso from "../dist/index.js";
import {
  CloseCode,
  NexAdmin,
  NexClient,
  NexHost,
  PROTOCOL_VERSION,
  describeClose,
  echoService,
  handlerService,
  tcpService,
  validateServices,
  wsService,
} from "../dist/node/index.js";

const cred = { account_id: "a", device_id: "d", device_token: "t" };

test("isomorphic entry exports the client API and nothing Node-only", () => {
  for (const name of ["NexClient", "NexApiError", "CloseCode", "PROTOCOL_VERSION", "describeClose"]) {
    assert.ok(name in iso, name);
  }
  for (const name of ["NexHost", "NexAdmin", "forwardTcp"]) assert.ok(!(name in iso), name);
  assert.equal(PROTOCOL_VERSION, 1);
  assert.deepEqual(
    Object.values(CloseCode).sort(),
    [4400, 4401, 4404, 4409, 4429, 4502, 4504],
  );
});

test("URLs keep the path prefix and switch scheme", () => {
  const c = new NexClient({ server: "https://relay.example.com/nex", credential: cred });
  assert.equal(c.server, "https://relay.example.com/nex/");
  assert.equal(c.wsUrl("/v1/connect?ticket=abc"), "wss://relay.example.com/nex/v1/connect?ticket=abc");
  assert.equal(c.httpUrl("v1/devices"), "https://relay.example.com/nex/v1/devices");
  const p = new NexClient({ server: "http://127.0.0.1:8080", credential: cred });
  assert.equal(p.wsUrl("v1/host"), "ws://127.0.0.1:8080/v1/host");
  const slash = new NexClient({ server: "http://h/nex/", credential: cred });
  assert.equal(slash.wsUrl("/v1/host"), "ws://h/nex/v1/host");
  assert.throws(() => new NexClient({ server: "ftp://x", credential: cred }), /http/);
});

test("requests go to the prefixed URL with the right auth", async () => {
  const seen = [];
  const fakeFetch = async (url, init) => {
    seen.push({ url: String(url), init });
    return new Response(JSON.stringify({ devices: [] }), { status: 200 });
  };
  const c = new NexClient({ server: "https://x.com/nex", credential: cred, fetch: fakeFetch });
  await c.devices();
  assert.equal(seen[0].url, "https://x.com/nex/v1/devices");
  assert.equal(seen[0].init.headers.authorization, "Bearer t");

  const admin = new NexAdmin({ server: "https://x.com/nex", adminKey: "k", fetch: fakeFetch });
  await admin.devices("user/42 ü");
  assert.equal(seen[1].url, "https://x.com/nex/v1/admin/accounts/user%2F42%20%C3%BC/devices");
  assert.equal(seen[1].init.headers["x-nex-admin-key"], "k");
});

test("API errors expose status and the relay's message", async () => {
  const c = new NexClient({
    server: "http://x",
    credential: cred,
    fetch: async () => new Response(JSON.stringify({ error: "nope" }), { status: 409 }),
  });
  await assert.rejects(c.whoami(), (err) => err instanceof iso.NexApiError && err.status === 409 && err.message === "nope");
});

test("service descriptors validate names and targets", () => {
  assert.deepEqual(tcpService("ssh", "127.0.0.1:22").target, { type: "tcp", host: "127.0.0.1", port: 22 });
  assert.deepEqual(tcpService("db", "[::1]:5432").target, { type: "tcp", host: "::1", port: 5432 });
  assert.equal(wsService("agl", "ws://127.0.0.1:8790/ws").kind, "ws");
  assert.equal(echoService().name, "echo");
  assert.equal(handlerService("x", "tcp", () => {}).kind, "tcp");
  assert.throws(() => tcpService("bad name", "127.0.0.1:22"), /invalid service name/);
  assert.throws(() => tcpService("a".repeat(65), "127.0.0.1:22"), /invalid service name/);
  assert.throws(() => tcpService("ok", "127.0.0.1"), /HOST:PORT/);
  assert.throws(() => wsService("ok", "http://x"), /ws:/);
  assert.throws(() => handlerService("ok", "udp", () => {}), /kind/);
  assert.throws(() => validateServices([echoService(), echoService()]), /duplicate/);
  const many = Array.from({ length: 65 }, (_, i) => echoService(`e${i}`));
  assert.throws(() => validateServices(many), /at most 64/);
  const client = new NexClient({ server: "http://x", credential: cred });
  assert.throws(() => new NexHost({ client, services: [echoService(), echoService()] }), /duplicate/);
});

test("describeClose explains relay close codes", () => {
  assert.match(describeClose(4404), /offline/);
  assert.match(describeClose(4502, "local service unavailable: x"), /rejected.*local service unavailable/);
  assert.match(describeClose(4504), /in time/);
  assert.match(describeClose(4429), /too many/);
  assert.match(describeClose(1000), /closed/);
});
