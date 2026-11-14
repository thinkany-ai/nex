// End-to-end against a real nex-server: three devices in one account reaching
// each other's tcp / ws / echo / in-process services at the same time, plus
// pairing, rejections, cross-account isolation, replacement and revocation.
// Mirrors crates/nex/tests/e2e.rs.

import assert from "node:assert/strict";
import { after, before, test } from "node:test";
import {
  CloseCode,
  NexApiError,
  NexClient,
  NexFatalError,
  NexHost,
  echoService,
  forwardTcp,
  handlerService,
  openStream,
  tcpService,
  wsService,
} from "../dist/node/index.js";
import {
  runHost,
  startRelay,
  tcpClientRoundtrip,
  tcpEcho,
  tcpRoundtrip,
  text,
  waitFor,
  waitOnline,
  watch,
  within,
  wsEcho,
  wsRoundtrip,
} from "./helpers.mjs";

let relay;
const cleanups = [];
const s = {}; // shared state across the ordered tests below

before(async () => {
  relay = await startRelay();
});

after(async () => {
  for (const fn of cleanups.reverse()) await fn().catch(() => {});
  await relay?.stop();
});

test("bootstrap and pairing (single use, case/dash-insensitive)", async () => {
  const laptopCred = await NexClient.bootstrap(relay.url, { accountName: "team", deviceName: "laptop", app: "termany" });
  assert.match(laptopCred.device_token, /^nexd_/);
  s.laptop = new NexClient({ server: relay.url, credential: laptopCred });
  assert.deepEqual(
    { ...(await s.laptop.whoami()) },
    { account_id: laptopCred.account_id, device_id: laptopCred.device_id, device_name: "laptop", app: "termany" },
  );

  const code = await s.laptop.createPairingCode();
  assert.match(code.code, /^[A-Z0-9]{4}-[A-Z0-9]{4}$/);
  s.vps = new NexClient({ server: relay.url, credential: await NexClient.claim(relay.url, { code: code.code, deviceName: "vps" }) });
  await assert.rejects(NexClient.claim(relay.url, { code: code.code, deviceName: "again" }), (e) => e instanceof NexApiError && e.status === 404);

  const code2 = await s.laptop.createPairingCode();
  const typed = code2.code.replace("-", "").toLowerCase();
  s.phone = new NexClient({ server: relay.url, credential: await NexClient.claim(relay.url, { code: typed, deviceName: "phone", app: "dscode" }) });
  assert.equal(s.phone.credential.account_id, laptopCred.account_id);
});

test("hosts expose tcp, ws, echo and handler services; reached concurrently", async () => {
  const laptopShell = await tcpEcho("laptop:");
  const laptopAgl = await wsEcho("laptop-agl:", { greeting: "hello-from-agl" });
  const vpsShell = await tcpEcho("vps:");
  cleanups.push(laptopShell.close, laptopAgl.close, vpsShell.close);

  const onlineIds = [];
  s.laptopHost = new NexHost({
    client: s.laptop,
    services: [
      tcpService("shell", laptopShell.addr),
      wsService("agl", laptopAgl.url),
      echoService(),
      // In-process service: greets first, then upper-cases text and reverses binary.
      handlerService("term", "ws", (socket, info) => {
        socket.send(`hi ${info.fromDeviceId}`);
        socket.on("message", (data, isBinary) => {
          if (isBinary) socket.send(Buffer.from(data).reverse(), { binary: true });
          else socket.send(data.toString().toUpperCase());
        });
      }),
    ],
    onOnline: (id) => onlineIds.push(id),
  });
  s.laptopRun = runHost(s.laptopHost);
  s.vpsHost = new NexHost({ client: s.vps, services: [tcpService("shell", vpsShell.addr)] });
  s.vpsRun = runHost(s.vpsHost);
  cleanups.push(s.laptopRun.stop, s.vpsRun.stop);
  await waitOnline(s.phone, s.laptop.deviceId);
  await waitOnline(s.phone, s.vps.deviceId);
  assert.deepEqual(onlineIds, [s.laptop.deviceId]);
  assert.equal(s.laptopHost.online, true);

  const listed = await s.phone.devices();
  assert.equal(listed.length, 3);
  const me = listed.find((d) => d.is_self);
  assert.equal(me.name, "phone");
  assert.equal(me.online, false);
  const l = listed.find((d) => d.name === "laptop");
  assert.deepEqual(
    l.services.map((x) => `${x.name}:${x.kind}`).sort(),
    ["agl:ws", "echo:ws", "shell:tcp", "term:ws"],
  );

  // Many streams at once, every direction, by name and by id.
  const results = await Promise.all([
    tcpRoundtrip(s.phone, "laptop", "shell", "hello", "laptop:hello"),
    tcpRoundtrip(s.phone, s.vps.deviceId, "shell", "hi", "vps:hi"),
    tcpRoundtrip(s.vps, "laptop", "shell", "from-vps", "laptop:from-vps"),
    tcpRoundtrip(s.laptop, "vps", "shell", "from-laptop", "vps:from-laptop"),
    wsRoundtrip(s.phone, "laptop", "echo", "ping"),
    ...Array.from({ length: 10 }, (_, i) => wsRoundtrip(s.vps, "laptop", "echo", `e${i}`)),
  ]);
  assert.deepEqual(results.slice(0, 5), ["laptop:hello", "vps:hi", "laptop:from-vps", "vps:from-laptop", "ping"]);
  assert.deepEqual(results.slice(5), Array.from({ length: 10 }, (_, i) => `e${i}`));

  // ws service: the greeting sent before the relay leg was up is not lost,
  // text stays text, binary stays binary, a long-lived stream carries many messages.
  const agl = await s.phone.open("laptop", "agl");
  assert.equal(agl.binaryType, "arraybuffer");
  const aw = watch(agl);
  assert.equal(await aw.next(), "hello-from-agl");
  for (let i = 0; i < 50; i++) {
    agl.send(`m${i}`);
    assert.equal(await aw.next(), `laptop-agl:m${i}`);
  }
  agl.send(new Uint8Array([1, 2, 3]));
  const bin = await aw.next();
  assert.ok(bin instanceof ArrayBuffer, "binary frame arrives as ArrayBuffer");
  assert.deepEqual([...new Uint8Array(bin)], [1, 2, 3]);
  agl.close();

  // echo: binary too.
  const ew = await s.phone.open("laptop", "echo");
  const eww = watch(ew);
  ew.send(new Uint8Array([9, 8, 7]));
  assert.deepEqual([...new Uint8Array(await eww.next())], [9, 8, 7]);
  ew.close();

  // handler service, via the Node `ws` flavour as well.
  const term = await s.phone.open("laptop", "term");
  const tw = watch(term);
  assert.equal(await tw.next(), `hi ${s.phone.deviceId}`);
  term.send("abc");
  assert.equal(await tw.next(), "ABC");
  term.send(new Uint8Array([1, 2, 3]));
  assert.deepEqual([...new Uint8Array(await tw.next())], [3, 2, 1]);
  term.close();

  const nodeWs = await openStream(s.vps, "laptop", "term");
  const greeting = await new Promise((resolve) => nodeWs.once("message", (d) => resolve(d.toString())));
  assert.equal(greeting, `hi ${s.vps.deviceId}`);
  nodeWs.close();

  // A big tcp payload in both directions (backpressure path).
  const big = await s.phone.open("vps", "shell");
  const bw = watch(big);
  const chunk = new Uint8Array(256 * 1024).fill(65);
  let received = 0;
  for (let i = 0; i < 8; i++) big.send(chunk);
  const want = 8 * chunk.length; // each read is prefixed with "vps:", so at least this much comes back
  while (received < want) received += (await bw.next()).byteLength;
  big.close();
});

test("forwardTcp tunnels local TCP connections, concurrently", async () => {
  const server = await forwardTcp(s.phone, "vps", "shell");
  cleanups.push(() => new Promise((r) => server.close(r)));
  const port = server.address().port;
  const replies = await Promise.all(
    Array.from({ length: 8 }, (_, i) => tcpClientRoundtrip(port, `fwd${i}`, `vps:fwd${i}`.length)),
  );
  assert.deepEqual(replies, Array.from({ length: 8 }, (_, i) => `vps:fwd${i}`));

  // Also to the laptop, by id, while the vps forward is in use.
  const server2 = await forwardTcp(s.vps, s.laptop.deviceId, "shell", { host: "127.0.0.1" });
  cleanups.push(() => new Promise((r) => server2.close(r)));
  const [a, b] = await Promise.all([
    tcpClientRoundtrip(server2.address().port, "x", "laptop:x".length),
    tcpClientRoundtrip(port, "y", "vps:y".length),
  ]);
  assert.deepEqual([a, b], ["laptop:x", "vps:y"]);
});

test("closing the local TCP side ends the relay stream", async () => {
  const sockets = [];
  const { default: net } = await import("node:net");
  const srv = net.createServer((sock) => {
    sockets.push(sock);
    sock.on("error", () => {});
  });
  await new Promise((r) => srv.listen(0, "127.0.0.1", r));
  cleanups.push(() => new Promise((r) => srv.close(r)));
  s.laptopHost.updateServices([...s.laptopHost.services, tcpService("closer", `127.0.0.1:${srv.address().port}`)]);
  await waitFor(async () => (await s.phone.devices()).find((d) => d.name === "laptop").services.some((x) => x.name === "closer"));

  const ws = await s.phone.open("laptop", "closer");
  const w = watch(ws);
  await waitFor(() => sockets.length === 1, { what: "local tcp connection" });
  sockets[0].end();
  const closed = await w.closed();
  assert.ok(closed.code < 4000, `plain close, got ${closed.code}`);

  // And the other way: the stream closing ends the local TCP connection.
  const ws2 = await s.phone.open("laptop", "closer");
  await waitFor(() => sockets.length === 2, { what: "second local tcp connection" });
  const ended = new Promise((r) => sockets[1].once("close", r));
  ws2.close();
  await within(5000, ended, "local tcp close");
});

test("dead local service is rejected with 4502", async () => {
  const { freePort } = await import("./helpers.mjs");
  const dead = await freePort();
  s.laptopHost.updateServices([...s.laptopHost.services, tcpService("db", `127.0.0.1:${dead}`), wsService("deadws", `ws://127.0.0.1:${dead}/`)]);
  await waitFor(async () => (await s.phone.devices()).find((d) => d.name === "laptop").services.some((x) => x.name === "deadws"));
  for (const svc of ["db", "deadws"]) {
    const ws = await s.phone.open("laptop", svc);
    const closed = await watch(ws).closed();
    assert.equal(closed.code, CloseCode.REJECTED);
    assert.match(closed.reason, /unavailable/);
  }
});

test("unknown service, offline device, used ticket", async () => {
  await assert.rejects(s.phone.ticket("laptop", "nope"), (e) => e instanceof NexApiError && e.status === 404 && /does not expose/.test(e.message));
  await assert.rejects(s.laptop.ticket("phone", "shell"), (e) => e.status === 404 && /offline/.test(e.message));
  await assert.rejects(s.phone.open("phone", "shell"), (e) => e.status === 404);
  await assert.rejects(s.phone.ticket("nobody", "shell"), (e) => e.status === 404);

  // A ticket works exactly once.
  const ticket = await s.phone.ticket("vps", "shell");
  const url = s.phone.wsUrl(ticket.path);
  const first = new WebSocket(url);
  await new Promise((resolve, reject) => {
    first.onopen = resolve;
    first.onerror = reject;
  });
  first.close();
  const second = new WebSocket(url);
  const c = await watch(second).closed();
  assert.equal(c.code, 1006, "handshake refused");
});

test("ticket taken while online, device goes offline: 4404", async () => {
  const code = await s.laptop.createPairingCode();
  const tmp = new NexClient({ server: relay.url, credential: await NexClient.claim(relay.url, { code: code.code, deviceName: "tmp" }) });
  const h = runHost(new NexHost({ client: tmp, services: [echoService()] }));
  await waitOnline(s.phone, tmp.deviceId);
  const ticket = await s.phone.ticket("tmp", "echo");
  await h.stop();
  await waitOnline(s.phone, tmp.deviceId, false);
  const ws = new WebSocket(s.phone.wsUrl(ticket.path));
  const c = await watch(ws).closed();
  assert.equal(c.code, CloseCode.OFFLINE);
});

test("another account cannot see or reach these devices", async () => {
  const other = new NexClient({ server: relay.url, credential: await NexClient.bootstrap(relay.url, { accountName: "other", deviceName: "x" }) });
  assert.equal((await other.devices()).length, 1);
  await assert.rejects(other.ticket(s.laptop.deviceId, "shell"), (e) => e.status === 404);
  await assert.rejects(other.revoke(s.laptop.deviceId), (e) => e.status === 404);
  await assert.rejects(other.rename(s.laptop.deviceId, "pwned"), (e) => e.status === 404);
});

test("revoke: host run() rejects with 'revoked', streams close, calls get 401", async () => {
  const open = await s.phone.open("vps", "shell");
  const ow = watch(open);
  await s.laptop.revoke(s.vps.deviceId);
  await assert.rejects(within(5000, s.vpsRun.done, "revoked host"), (e) => e instanceof NexFatalError && e.code === "revoked");
  await ow.closed();
  await assert.rejects(s.vps.devices(), (e) => e instanceof NexApiError && e.status === 401);
  await assert.rejects(s.phone.ticket("vps", "shell"), (e) => e.status === 404);
  assert.deepEqual((await s.phone.devices()).map((d) => d.name).sort(), ["laptop", "phone", "tmp"]);

  // Starting a host with a revoked credential fails at the handshake.
  const again = new NexHost({ client: s.vps, services: [echoService()] });
  await assert.rejects(within(5000, again.run(), "unauthorized host"), (e) => e instanceof NexFatalError && e.code === "unauthorized");
});

test("a second host with the same credential replaces the first", async () => {
  const offline = [];
  s.laptopHost.on("offline", (e) => offline.push(e));
  const second = new NexHost({ client: s.laptop, services: [echoService(), handlerService("v2", "ws", (ws) => ws.send("v2"))] });
  const secondRun = runHost(second);
  cleanups.push(secondRun.stop);
  await assert.rejects(within(5000, s.laptopRun.done, "replaced host"), (e) => e instanceof NexFatalError && e.code === "replaced");
  assert.equal(offline.length, 1);
  assert.equal(offline[0].code, "replaced");
  await waitFor(async () => (await s.phone.devices()).find((d) => d.name === "laptop").services.some((x) => x.name === "v2"));
  const ws = await s.phone.open("laptop", "v2");
  assert.equal(text(await watch(ws).next()), "v2");
  ws.close();

  // Abort resolves run() and reports offline with null.
  let offlineArg;
  second.on("offline", (e) => (offlineArg = e));
  await secondRun.stop();
  assert.equal(offlineArg, null);
  assert.equal(second.online, false);
  await waitOnline(s.phone, s.laptop.deviceId, false);
});
