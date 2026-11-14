// Relay started with an admin key: guarded bootstrap and the admin API.
// Mirrors product_backend_issues_devices_for_its_users in crates/nex/tests/e2e.rs.

import assert from "node:assert/strict";
import { after, before, test } from "node:test";
import { NexAdmin, NexApiError, NexClient, NexHost, echoService } from "../dist/node/index.js";
import { runHost, startRelay, waitOnline, wsRoundtrip } from "./helpers.mjs";

const KEY = "secret-key";
let relay;
const cleanups = [];

before(async () => {
  relay = await startRelay({ adminKey: KEY });
});

after(async () => {
  for (const fn of cleanups.reverse()) await fn().catch(() => {});
  await relay?.stop();
});

const status = (code) => (e) => e instanceof NexApiError && e.status === code;

test("bootstrap needs the admin key when the relay has one", async () => {
  await assert.rejects(NexClient.bootstrap(relay.url, { accountName: "x", deviceName: "y" }), status(403));
  await assert.rejects(NexClient.bootstrap(relay.url, { accountName: "x", deviceName: "y", adminKey: "wrong" }), status(403));
  const cred = await NexClient.bootstrap(relay.url, { accountName: "x", deviceName: "y", adminKey: KEY });
  assert.match(cred.account_id, /^acc_/);
});

test("admin API: same external id -> same account, list, rename, revoke, wrong key", async () => {
  const admin = new NexAdmin({ server: relay.url, adminKey: KEY });
  const issue = (user, device, app) =>
    admin.issueDevice({ externalAccountId: user, accountName: `${user} account`, deviceName: device, app });

  const macCred = await issue("user/42", "mac", "termany");
  const phoneCred = await issue("user/42", "phone", "dscode");
  const strangerCred = await issue("user/7", "pc", "termany");
  assert.equal(macCred.account_id, phoneCred.account_id);
  assert.notEqual(macCred.account_id, strangerCred.account_id);

  const mac = new NexClient({ server: relay.url, credential: macCred });
  const phone = new NexClient({ server: relay.url, credential: phoneCred });
  const stranger = new NexClient({ server: relay.url, credential: strangerCred });

  const me = await phone.whoami();
  assert.deepEqual([me.device_name, me.app], ["phone", "dscode"]);

  const h = runHost(new NexHost({ client: mac, services: [echoService()] }));
  cleanups.push(h.stop);
  await waitOnline(phone, mac.deviceId);
  assert.equal(await wsRoundtrip(phone, "mac", "echo", "hi"), "hi");
  await assert.rejects(stranger.ticket(mac.deviceId, "echo"), status(404));

  await phone.rename(mac.deviceId, "studio");
  const listed = await admin.devices("user/42");
  const studio = listed.find((d) => d.id === mac.deviceId);
  assert.deepEqual([studio.name, studio.app, studio.online], ["studio", "termany", true]);
  assert.ok(listed.every((d) => !d.is_self));
  await assert.rejects(stranger.rename(mac.deviceId, "x"), status(404));

  // Two devices with the same name: tickets by name are ambiguous (409), by id fine.
  const dup = await issue("user/42", "studio", "workany");
  await assert.rejects(phone.ticket("studio", "echo"), status(409));
  assert.equal((await phone.ticket(mac.deviceId, "echo")).device_id, mac.deviceId);
  await admin.revoke("user/42", dup.device_id);

  await admin.revoke("user/42", phone.deviceId);
  await assert.rejects(phone.whoami(), status(401));
  await assert.rejects(admin.revoke("user/7", mac.deviceId), status(404));
  await assert.rejects(admin.devices("nobody"), status(404));

  const wrong = new NexAdmin({ server: relay.url, adminKey: "nope" });
  await assert.rejects(wrong.devices("user/42"), status(403));
  await assert.rejects(wrong.issueDevice({ externalAccountId: "u", deviceName: "d" }), status(403));
});

test("admin API is off on a relay without a key", async () => {
  const open = await startRelay();
  cleanups.push(open.stop);
  const admin = new NexAdmin({ server: open.url, adminKey: "anything" });
  await assert.rejects(admin.devices("u"), status(403));
});
