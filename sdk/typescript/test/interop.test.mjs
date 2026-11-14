// Interop with the Rust CLI: a Rust `nex up --echo` host reached by the TS
// client, and a TS NexHost echo reached by `nex ping` / `nex forward`.

import assert from "node:assert/strict";
import { execFile, spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { after, before, test } from "node:test";
import { promisify } from "node:util";
import { NexClient, NexHost, echoService, handlerService, openStream } from "../dist/node/index.js";
import { NEX_CLI, runHost, startRelay, waitOnline, watch } from "./helpers.mjs";

const run = promisify(execFile);
let relay;
let dir;
const cleanups = [];

before(async () => {
  if (!existsSync(NEX_CLI)) throw new Error(`${NEX_CLI} not found: run \`cargo build --release\``);
  relay = await startRelay();
  dir = await mkdtemp(path.join(os.tmpdir(), "nex-ts-interop-"));
});

after(async () => {
  for (const fn of cleanups.reverse()) await fn().catch(() => {});
  await relay?.stop();
  if (dir) await rm(dir, { recursive: true, force: true });
});

async function writeConfig(name, credential) {
  const file = path.join(dir, `${name}.json`);
  await writeFile(file, JSON.stringify({ server: relay.url, credential }), { mode: 0o600 });
  return file;
}

const cliEnv = (config) => ({ ...process.env, NEX_CONFIG: config, RUST_LOG: "warn" });

test("TS client reaches a Rust `nex up --echo` host", async () => {
  const tsCred = await NexClient.bootstrap(relay.url, { accountName: "interop", deviceName: "ts-client" });
  const ts = new NexClient({ server: relay.url, credential: tsCred });
  const code = await ts.createPairingCode();

  // The Rust CLI joins with the pairing code (claim interop) and goes online.
  const rustConfig = path.join(dir, "rust-host.json");
  await run(NEX_CLI, ["join", code.code, "--server", relay.url, "--name", "rust-host"], { env: cliEnv(rustConfig) });
  const proc = spawn(NEX_CLI, ["up", "--echo"], { env: cliEnv(rustConfig), stdio: "ignore" });
  cleanups.push(async () => {
    if (proc.exitCode === null) {
      const gone = new Promise((r) => proc.once("exit", r));
      proc.kill("SIGKILL");
      await gone;
    }
  });
  const rustId = (await ts.devices()).find((d) => d.name === "rust-host").id;
  await waitOnline(ts, rustId);

  // Browser-style WebSocket: text and binary are echoed with their type.
  const ws = await ts.open("rust-host", "echo");
  const w = watch(ws);
  ws.send("hello rust");
  assert.equal(await w.next(), "hello rust");
  ws.send(new Uint8Array([0, 255, 1]));
  assert.deepEqual([...new Uint8Array(await w.next())], [0, 255, 1]);
  ws.close();

  // Node `ws` flavour, concurrently.
  const replies = await Promise.all(
    Array.from({ length: 5 }, async (_, i) => {
      const s = await openStream(ts, rustId, "echo");
      const reply = new Promise((r) => s.once("message", (d) => r(d.toString())));
      s.send(`n${i}`);
      const got = await reply;
      s.close();
      return got;
    }),
  );
  assert.deepEqual(replies, ["n0", "n1", "n2", "n3", "n4"]);
});

test("`nex ping` reaches a TS NexHost echo; `nex forward` reaches a TS handler", async () => {
  const hostCred = await NexClient.bootstrap(relay.url, { accountName: "interop2", deviceName: "ts-host" });
  const hostClient = new NexClient({ server: relay.url, credential: hostCred });
  const host = new NexHost({
    client: hostClient,
    services: [
      echoService(),
      // A tcp-kind service served in-process: echoes bytes with a prefix.
      handlerService("tcpecho", "tcp", (socket) => {
        socket.on("message", (data) => socket.send(Buffer.concat([Buffer.from("ts:"), data]), { binary: true }));
      }),
    ],
  });
  const h = runHost(host);
  cleanups.push(h.stop);

  const code = await hostClient.createPairingCode();
  const pingerCred = await NexClient.claim(relay.url, { code: code.code, deviceName: "rust-pinger" });
  const pingerConfig = await writeConfig("pinger", pingerCred);
  await waitOnline(new NexClient({ server: relay.url, credential: pingerCred }), hostCred.device_id);

  const { stdout } = await run(NEX_CLI, ["ping", "ts-host", "-c", "3", "-i", "0.01"], {
    env: cliEnv(pingerConfig),
    timeout: 20_000,
  });
  assert.match(stdout, /seq=2 rtt=/);
  assert.match(stdout, /3 pings: min/);

  // `nex forward` to the in-process tcp handler.
  const fwd = spawn(NEX_CLI, ["forward", "ts-host/tcpecho", "--listen", "127.0.0.1:0"], {
    env: cliEnv(pingerConfig),
    stdio: ["ignore", "ignore", "pipe"],
  });
  cleanups.push(async () => {
    if (fwd.exitCode === null) {
      const gone = new Promise((r) => fwd.once("exit", r));
      fwd.kill("SIGKILL");
      await gone;
    }
  });
  const port = await new Promise((resolve, reject) => {
    let buf = "";
    fwd.stderr.on("data", (d) => {
      buf += d;
      const m = /forwarding 127\.0\.0\.1:(\d+)/.exec(buf);
      if (m) resolve(Number(m[1]));
    });
    fwd.once("exit", () => reject(new Error(`nex forward exited: ${buf}`)));
  });
  const { tcpClientRoundtrip } = await import("./helpers.mjs");
  assert.equal(await tcpClientRoundtrip(port, "abc", "ts:abc".length), "ts:abc");
});
