'use strict';
// Process-exit probe: a normal process that loads the addon, round-trips,
// and closes must terminate on its own (no hanging libuv handles/threads).
const assert = require('node:assert/strict');
const path = require('node:path');

const ADDON = process.env.UBM_NAPI_ADDON ||
  path.join(__dirname, '..', 'ubm_echo.linux-x64.node');
const addon = require(ADDON);

const session = new addon.EchoSession(addon.echoRevision());
assert.deepEqual([...session.echoBytes(Buffer.from([1, 2, 3]))], [1, 2, 3]);
session.echoBytesAsync(Buffer.from([4]), 4).then(() => {
  session.close();
  console.log('napi-exit-probe: OK');
});
