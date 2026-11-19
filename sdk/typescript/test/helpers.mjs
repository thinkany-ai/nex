// Shared test helpers: a real nex-server per test file, local echo services,
// and small promise wrappers around browser-style WebSockets.

import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import net from "node:net";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { WebSocketServer } from "ws";

const here = path.dirname(fileURLToPath(import.meta.url));
export const REPO = path.resolve(here, "../../..");
export const NEX_SERVER = process.env.NEX_SERVER_BIN ?? path.join(REPO, "target/release/nex-server");
export const NEX_CLI = process.env.NEX_CLI_BIN ?? path.join(REPO, "target/release/nex");

export function freePort() {
  return new Promise((resolve, reject) => {
    const srv = net.createServer();
    srv.unref();
    srv.on("error", reject);
    srv.listen(0, "127.0.0.1", () => {
      const { port } = srv.address();
      srv.close(() => resolve(port));
    });
  });
}

const delay = (ms) => new Promise((r) => setTimeout(r, ms));

/** Polls `fn` until it returns a truthy value (or throws after `timeoutMs`). */
export async function waitFor(fn, { timeoutMs = 5000, intervalMs = 20, what = "condition" } = {}) {
  const deadline = Date.now() + timeoutMs;
  let lastErr;
  while (Date.now() < deadline) {
    try {
      const v = await fn();
      if (v) return v;
    } catch (err) {
      lastErr = err;
    }
    await delay(intervalMs);
  }
  throw new Error(`timed out waiting for ${what}${lastErr ? `: ${lastErr.message}` : ""}`);
}

export const TEST_DATABASE_URL = process.env.NEX_TEST_DATABASE_URL ?? "postgres://localhost:5432/nex_test";

/** Starts `nex-server` on a free port with a fresh schema in the test database. */
export async function startRelay({ adminKey } = {}) {
  if (!existsSync(NEX_SERVER)) {
    throw new Error(`${NEX_SERVER} not found: run \`cargo build --release\` in ${REPO}`);
  }
  for (let attempt = 0; attempt < 5; attempt++) {
    const port = await freePort();
    // Each relay gets its own schema in the test database.
    const schema = `t_ts_${process.pid}_${Date.now()}_${attempt}`;
    const args = ["--listen", `127.0.0.1:${port}`, "--database-url", TEST_DATABASE_URL, "--database-schema", schema];
    if (adminKey) args.push("--admin-key", adminKey);
    else args.push("--open-bootstrap");
    const proc = spawn(NEX_SERVER, args, { stdio: ["ignore", "ignore", "pipe"], env: { ...process.env, RUST_LOG: "warn" } });
    let stderr = "";
    proc.stderr.on("data", (d) => (stderr += d));
    let exited = false;
    proc.once("exit", () => (exited = true));
    const url = `http://127.0.0.1:${port}`;
    try {
      await waitFor(
        async () => {
          if (exited) throw new Error("exited");
          const r = await fetch(`${url}/healthz`);
          return r.ok && (await r.text()) === "ok";
        },
        { timeoutMs: 10_000, what: "relay /healthz" },
      );
    } catch (err) {
      proc.kill("SIGKILL");
      if (exited && attempt < 4) continue; // port race: try another
      throw new Error(`relay did not start: ${err.message}\n${stderr}`);
    }
    return {
      url,
      proc,
      async stop() {
        if (proc.exitCode !== null) return;
        const gone = new Promise((r) => proc.once("exit", r));
        proc.kill("SIGKILL");
        await gone;
      },
    };
  }
  throw new Error("unreachable");
}

/** TCP echo server that prefixes replies, so services can be told apart. */
export async function tcpEcho(prefix) {
  const sockets = new Set();
  const server = net.createServer((sock) => {
    sockets.add(sock);
    sock.on("close", () => sockets.delete(sock));
    sock.on("error", () => {});
    sock.on("data", (d) => sock.write(Buffer.concat([Buffer.from(prefix), d])));
  });
  await new Promise((r) => server.listen(0, "127.0.0.1", r));
  return {
    addr: `127.0.0.1:${server.address().port}`,
    close: () => {
      for (const s of sockets) s.destroy();
      return new Promise((r) => server.close(r));
    },
  };
}

/**
 * WebSocket echo server: prefixes text replies, returns binary as binary, and
 * greets every new connection immediately (to exercise early-message buffering).
 */
export async function wsEcho(prefix, { greeting } = {}) {
  const wss = new WebSocketServer({ host: "127.0.0.1", port: 0 });
  await new Promise((r) => wss.once("listening", r));
  wss.on("connection", (ws) => {
    if (greeting) ws.send(greeting);
    ws.on("message", (data, isBinary) => {
      if (isBinary) ws.send(data, { binary: true });
      else ws.send(prefix + data.toString());
    });
  });
  return {
    url: `ws://127.0.0.1:${wss.address().port}/ws`,
    close: () => {
      for (const c of wss.clients) c.terminate();
      return new Promise((r) => wss.close(r));
    },
  };
}

/** Collects messages / the close event of a browser-style WebSocket. */
export function watch(ws) {
  const queue = [];
  const waiters = [];
  let closed = null;
  const closeWaiters = [];
  ws.addEventListener("message", (ev) => {
    const w = waiters.shift();
    if (w) w.resolve(ev.data);
    else queue.push(ev.data);
  });
  ws.addEventListener("close", (ev) => {
    closed = { code: ev.code, reason: ev.reason };
    for (const w of waiters.splice(0)) w.reject(new Error(`closed ${ev.code} ${ev.reason}`));
    for (const r of closeWaiters.splice(0)) r(closed);
  });
  return {
    next(timeoutMs = 5000) {
      if (queue.length) return Promise.resolve(queue.shift());
      if (closed) return Promise.reject(new Error(`closed ${closed.code} ${closed.reason}`));
      return new Promise((resolve, reject) => {
        const t = setTimeout(() => reject(new Error("no message in time")), timeoutMs);
        waiters.push({
          resolve: (v) => (clearTimeout(t), resolve(v)),
          reject: (e) => (clearTimeout(t), reject(e)),
        });
      });
    },
    closed(timeoutMs = 5000) {
      if (closed) return Promise.resolve(closed);
      return new Promise((resolve, reject) => {
        const t = setTimeout(() => reject(new Error("socket did not close in time")), timeoutMs);
        closeWaiters.push((c) => (clearTimeout(t), resolve(c)));
      });
    },
  };
}

export const text = (data) => (typeof data === "string" ? data : Buffer.from(data).toString());

/** Opens a tcp-service stream, sends `payload` as binary, reads `expectLen` bytes. */
export async function tcpRoundtrip(client, device, service, payload, expect) {
  const ws = await client.open(device, service);
  const w = watch(ws);
  ws.send(new TextEncoder().encode(payload));
  let got = "";
  while (got.length < expect.length) {
    const d = await w.next();
    if (typeof d === "string") throw new Error("tcp stream delivered a text frame");
    got += Buffer.from(d).toString();
  }
  ws.close();
  return got;
}

/** Opens a ws-service stream, sends one text message and returns the reply. */
export async function wsRoundtrip(client, device, service, msg) {
  const ws = await client.open(device, service);
  const w = watch(ws);
  ws.send(msg);
  const reply = await w.next();
  ws.close();
  return text(reply);
}

export async function waitOnline(observer, deviceId, online = true) {
  await waitFor(
    async () => (await observer.devices()).some((d) => d.id === deviceId && d.online === online),
    { what: `device ${deviceId} ${online ? "online" : "offline"}` },
  );
}

/** Starts `host.run()` with an AbortController; returns { stop, done }. */
export function runHost(host) {
  const ac = new AbortController();
  const done = host.run(ac.signal);
  done.catch(() => {}); // asserted explicitly where relevant
  return {
    done,
    async stop() {
      ac.abort();
      await done.catch(() => {});
    },
  };
}

/** Rejects if `promise` does not settle within `ms`. */
export function within(ms, promise, what = "operation") {
  let t;
  return Promise.race([
    promise.finally(() => clearTimeout(t)),
    new Promise((_, reject) => {
      t = setTimeout(() => reject(new Error(`${what} did not finish within ${ms} ms`)), ms);
    }),
  ]);
}

/** Connects a raw TCP client, writes `payload`, reads until `expect` bytes arrive. */
export function tcpClientRoundtrip(port, payload, expectLen) {
  return new Promise((resolve, reject) => {
    const sock = net.connect({ host: "127.0.0.1", port });
    let got = Buffer.alloc(0);
    const t = setTimeout(() => (sock.destroy(), reject(new Error("tcp client timeout"))), 5000);
    sock.on("error", (e) => (clearTimeout(t), reject(e)));
    sock.on("connect", () => sock.write(payload));
    sock.on("data", (d) => {
      got = Buffer.concat([got, d]);
      if (got.length >= expectLen) {
        clearTimeout(t);
        sock.end();
        resolve(got.toString());
      }
    });
  });
}
