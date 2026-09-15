'use strict';
// N-API feasibility round-trip: loads the real built addon and proves the
// exchange (typed errors, owned bytes, lossless u64, async cancellation,
// callback invalidation, panic containment). Fails loudly; no skips.
const assert = require('node:assert/strict');
const path = require('node:path');

const ADDON = process.env.UBM_NAPI_ADDON ||
  path.join(__dirname, '..', 'ubm_echo.linux-x64.node');
const addon = require(ADDON);

const REV = 'C-UBM.0.1.1-DRAFT';
const MAX = 524288;

function codeOf(err) {
  const m = /^([^|]+)\|([^|]+)\|([^|]+)\|/.exec(String(err && err.message));
  assert.ok(m, `error must carry code|domain|operation wire form, got: ${err && err.message}`);
  return { code: m[1], domain: m[2], operation: m[3] };
}

async function rejectsWithCode(promise, code, domain) {
  await assert.rejects(promise, err => {
    const got = codeOf(err);
    return got.code === code && got.domain === domain;
  }, `expected rejection ${code}|${domain}`);
}

async function main() {
  // PKG-01 artifact identity.
  assert.equal(addon.echoRevision(), REV);
  assert.equal(addon.echoMaxBytes(), MAX);

  // PKG-02 init contract: foreign revision fails closed.
  await rejectsWithCode(
    (async () => new addon.EchoSession('C-UBM.9.9.9-DRAFT'))(),
    'protocol.incompatible', 'core');

  const session = new addon.EchoSession(REV);

  // Owned byte batches.
  const input = Buffer.from([0, 1, 2, 250, 255]);
  const out = session.echoBytes(input);
  assert.ok(Buffer.isBuffer(out));
  assert.deepEqual([...out], [0, 1, 2, 250, 255]);
  input[0] = 99; // mutating the caller buffer must not affect prior output
  assert.equal(out[0], 0);
  assert.deepEqual([...session.echoBytes(Buffer.alloc(0))], []);

  const big = Buffer.alloc(MAX, 0xab);
  assert.equal(session.echoBytes(big).length, MAX);
  await rejectsWithCode(
    (async () => session.echoBytes(Buffer.alloc(MAX + 1)))(),
    'bytes.too-large', 'core');

  // DATA-02 lossless u64 counters via BigInt <-> decimal string.
  for (const n of [0n, 1n, 9007199254740993n, 9223372036854775807n, 18446744073709551615n]) {
    const echoed = await BigInt(session.echoCounter(n.toString()));
    assert.equal(echoed, n, `counter ${n}`);
  }
  assert.equal(session.echoCounter('00042'), '42');
  for (const bad of ['', '-1', '+5', '12a34', ' 42', '4.0', '0x10', '18446744073709551616']) {
    await rejectsWithCode((async () => session.echoCounter(bad))(), 'bytes.invalid', 'core');
  }

  // Async success.
  const asyncOut = await session.echoBytesAsync(Buffer.from([7, 8, 9]), 4);
  assert.deepEqual([...asyncOut], [7, 8, 9]);
  await rejectsWithCode(session.echoBytesAsync(Buffer.from([1]), 0), 'argument.invalid', 'core');
  // L3: one async operation label. The pre-queue validation and the worker
  // share `echo-bytes-async`, so oversize async input carries that label.
  // (Pre-queue failures throw synchronously, like the other sync calls
  // above, hence the async-IIFE wrap; worker failures reject.)
  await assert.rejects((async () => session.echoBytesAsync(Buffer.alloc(MAX + 1), 4))(), err => {
    const got = codeOf(err);
    return got.code === 'bytes.too-large' && got.domain === 'core'
      && got.operation === 'echo-bytes-async';
  }, 'oversize async must carry echo-bytes-async operation');

  // Cancel-before-start aborts deterministically.
  session.cancelInflight();
  await rejectsWithCode(
    session.echoBytesAsync(Buffer.from([1, 2, 3]), 10),
    'operation.aborted', 'core');
  // Session usable after an aborted call.
  assert.deepEqual([...session.echoBytes(Buffer.from([9]))], [9]);

  // Mid-flight cancellation: the armed cancel deterministically aborts the
  // pending call (entry-take when the worker has not started, chunk-check
  // when it has), and the abort disarms so the session stays usable.
  // Atomics.wait parks the main thread while the worker progresses, then the
  // cancel lands mid-flight with a wide timing margin (work ~= seconds).
  const raceInput = Buffer.alloc(65536, 0x5a);
  const pending = session.echoBytesAsync(raceInput, 200000);
  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 50);
  session.cancelInflight();
  await rejectsWithCode(pending, 'operation.aborted', 'core');
  assert.deepEqual([...session.echoBytes(Buffer.from([1]))], [1]);

  // Close cancels in-flight work: the pending call aborts, later calls are dead.
  const session2 = new addon.EchoSession(REV);
  const doomed = session2.echoBytesAsync(Buffer.alloc(65536, 0x33), 200000);
  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 50);
  session2.close();
  await rejectsWithCode(doomed, 'operation.aborted', 'core');
  await rejectsWithCode((async () => session2.echoBytes(Buffer.from([1])))(),
    'lifecycle.destroyed', 'core');

  // Callback registration, delivery, and invalidation on close. The binding
  // uses the default CalleeHandled strategy: delivery is Node-style (err, value).
  const seen = [];
  const errs = [];
  session.onEvent((err, msg) => { errs.push(err); seen.push(msg); });
  session.emitTestEvent('hello');
  await new Promise(resolve => setImmediate(resolve));
  await new Promise(resolve => setImmediate(resolve));
  assert.deepEqual(errs, [null]);
  assert.deepEqual(seen, ['hello']);

  session.close();
  session.close(); // idempotent
  await rejectsWithCode((async () => session.emitTestEvent('after-close'))(),
    'lifecycle.destroyed', 'core');
  await new Promise(resolve => setImmediate(resolve));
  assert.deepEqual(seen, ['hello'], 'no delivery after close');
  await rejectsWithCode((async () => session.echoBytes(Buffer.from([1])))(),
    'lifecycle.destroyed', 'core');
  await rejectsWithCode((async () => session.echoCounter('1'))(),
    'lifecycle.destroyed', 'core');
  // M1: uniform post-close cancel. `close` invalidates EVERY later call, so
  // a post-close cancelInflight rejects `lifecycle.destroyed` (uniffi/JNI
  // agree; the close docstring stands).
  await assert.rejects((async () => session.cancelInflight())(), err => {
    const got = codeOf(err);
    return got.code === 'lifecycle.destroyed' && got.domain === 'core'
      && got.operation === 'cancel-inflight';
  }, 'post-close cancelInflight must reject lifecycle.destroyed');

  // Panic probes were deleted from production paths by the wiring slice:
  // no feasibility-only export may ship. Containment rests on the
  // `catch_unwind` attribute present on every export (see LIFETIME_RULES.md).
  assert.equal(typeof addon.__feasibilityPanicProbe, 'undefined',
    'panic probe must not exist on the production addon');

  console.log('napi-roundtrip: OK');
}

main().catch(err => {
  console.error(`napi-roundtrip: FAIL ${err && err.stack || err}`);
  process.exit(1);
});
