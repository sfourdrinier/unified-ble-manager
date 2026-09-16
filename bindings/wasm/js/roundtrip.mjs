import assert from 'node:assert/strict';
import fs from 'node:fs';

// Portable WASM exchange: instantiates the real built module with an EMPTY
// import object (zero-import portability proof) and drives the full protocol
// through linear memory. Fails loudly; no skips.
const WASM_PATH = process.argv[2];
assert.ok(WASM_PATH, 'usage: roundtrip.mjs <module.wasm>');

const REV = 'C-UBM.0.1.1-DRAFT';
const MAX = 524288;
const CODE = { OK: 0, ARG: 1, BYTES_INVALID: 2, TOO_LARGE: 3, ABORTED: 4, STATE: 5, INCOMPAT: 6, CAP: 7 };

const bytes = await fs.promises.readFile(WASM_PATH);
const mod = await WebAssembly.compile(bytes);

// Zero-import instantiation: the portable build must not require any host
// function (no Tokio/fs/radio shims, no JS glue imports).
const instance = await WebAssembly.instantiate(mod, {});
const ex = instance.exports;
for (const name of ['memory', 'ubm_echo_alloc', 'ubm_echo_free', 'ubm_echo_init',
  'ubm_echo_run', 'ubm_echo_counter', 'ubm_echo_last_error', 'ubm_echo_last_error_text',
  'ubm_echo_stream_begin', 'ubm_echo_stream_push', 'ubm_echo_stream_cancel',
  'ubm_echo_stream_finish', 'ubm_echo_describe_json',
  'ubm_echo_central_status', 'ubm_echo_expire_sweep', 'ubm_echo_destroy',
  'ubm_echo_ble_transition']) {
  assert.ok(ex[name], `missing export ${name}`);
}

let mem = () => new Uint8Array(ex.memory.buffer);
let view = () => new DataView(ex.memory.buffer);
// Byte-ownership ledger. Two pointer kinds cross the boundary: explicit
// allocs and module-published outputs. Every obtained pointer is freed
// exactly once through its kind's releaser; live counters must never go
// negative (double-free) and must end at zero (unbalanced bookkeeping).
// Null results (empty echoes) are never freed by construction.
let liveAlloc = 0;
let ownershipCycles = 0;
const enc = new TextEncoder(), dec = new TextDecoder();

function alloc(n) {
  const ptr = ex.ubm_echo_alloc(n);
  assert.notEqual(ptr, 0, `alloc(${n}) must succeed`);
  liveAlloc++;
  return ptr;
}
function freeAlloc(ptr, len) {
  if (ptr === 0) return;
  liveAlloc--;
  assert.ok(liveAlloc >= 0, 'double-free of an alloc');
  ex.ubm_echo_free(ptr, len);
  ownershipCycles++;
}
function takePublished(ptr, len) {
  // Single choke point for published buffers: copy out, then release.
  // Published pointers never escape this function, so each published buffer
  // is freed exactly once; a forgotten take would leak module memory, which
  // the growth probe at the end detects.
  if (ptr === 0) return null;
  const bytes = mem().slice(ptr, ptr + len);
  ex.ubm_echo_free(ptr, len);
  ownershipCycles++;
  return bytes;
}
function writeInput(bytesIn) {
  if (bytesIn.length === 0) return 0; // empty inputs cross as (null, 0)
  const ptr = alloc(bytesIn.length);
  mem().set(bytesIn, ptr);
  return ptr;
}
function readOut(ptr, len) {
  return mem().slice(ptr, ptr + len);
}
function lastText() {
  const scratch = alloc(8);
  const ptr = ex.ubm_echo_last_error_text(scratch);
  const len = view().getUint32(scratch, true);
  freeAlloc(scratch, 8);
  assert.notEqual(ptr, 0);
  return dec.decode(readOut(ptr, len)); // static slot: copied, never freed
}
function checkOk() {
  assert.equal(ex.ubm_echo_last_error(), CODE.OK,
    `expected no error, wired: ${ex.ubm_echo_last_error()}`);
}
function strBytes(s) { return enc.encode(s); }

// Scratch cell for out_len params.
const scratch = alloc(8);
const setOut = v => view().setUint32(scratch, v, true);
const getOut = () => view().getUint32(scratch, true);

function runBytes(input) {
  const inPtr = input.length === 0 ? 0 : writeInput(input);
  setOut(0xdeadbeef);
  const outPtr = ex.ubm_echo_run(inPtr, input.length, scratch);
  const outLen = getOut();
  if (inPtr !== 0) freeAlloc(inPtr, input.length);
  const out = takePublished(outPtr, outLen);
  if (out === null) return { ok: outLen === 0 && ex.ubm_echo_last_error() === 0, bytes: new Uint8Array(0) };
  return { ok: true, bytes: out };
}

function runCounter(decimal) {
  const data = strBytes(decimal);
  const inPtr = writeInput(data);
  setOut(0);
  const outPtr = ex.ubm_echo_counter(inPtr, data.length, scratch);
  const outLen = getOut();
  freeAlloc(inPtr, data.length);
  const out = takePublished(outPtr, outLen);
  if (out === null) return { ok: false };
  return { ok: true, text: dec.decode(out) };
}

function initWith(rev) {
  const data = strBytes(rev);
  const ptr = writeInput(data);
  const code = ex.ubm_echo_init(ptr, data.length);
  freeAlloc(ptr, data.length);
  return code;
}

// --- U7 transition-driving helpers (raw ABI, same shape as napi/uniffi/jni). ---
function statusJson() {
  setOut(0);
  const ptr = ex.ubm_echo_central_status(scratch);
  const len = getOut();
  if (ptr === 0) return { ok: false };
  return { ok: true, text: dec.decode(takePublished(ptr, len)) };
}
function sweep(nowDecimal) {
  const data = strBytes(nowDecimal);
  const inPtr = writeInput(data);
  const cell = alloc(8);
  view().setBigUint64(cell, 0n, true);
  const code = ex.ubm_echo_expire_sweep(inPtr, data.length, cell);
  const settled = view().getBigUint64(cell, true);
  freeAlloc(cell, 8);
  freeAlloc(inPtr, data.length);
  return { code, settled };
}
function destroyCentral() {
  setOut(0);
  const ptr = ex.ubm_echo_destroy(scratch);
  const len = getOut();
  if (ptr === 0) return { ok: false };
  return { ok: true, text: dec.decode(takePublished(ptr, len)) };
}
function bleTransition(name) {
  const data = strBytes(name);
  const ptr = writeInput(data);
  const code = ex.ubm_echo_ble_transition(ptr, data.length);
  freeAlloc(ptr, data.length);
  return code;
}

// --- Fresh instance: operations before init fail closed. ---
assert.equal(ex.ubm_echo_last_error(), CODE.OK);
{
  setOut(0);
  const inPtr = writeInput(new Uint8Array([1]));
  assert.equal(ex.ubm_echo_run(inPtr, 1, scratch), 0);
  freeAlloc(inPtr, 1);
  assert.equal(ex.ubm_echo_last_error(), CODE.STATE);
  assert.ok(lastText().startsWith('lifecycle.invalid-state|core|echo-bytes|'),
    lastText());
  assert.equal(ex.ubm_echo_stream_begin(), 0n);
  assert.equal(ex.ubm_echo_last_error(), CODE.STATE);
  // U7 driving fails closed before init like every other call.
  assert.equal(statusJson().ok, false, 'status before init must fail');
  assert.equal(ex.ubm_echo_last_error(), CODE.STATE);
  assert.ok(lastText().startsWith('lifecycle.invalid-state|core|central-status|'));
  assert.equal(sweep('0').code, CODE.STATE);
  assert.equal(destroyCentral().ok, false, 'destroy before init must fail');
  assert.equal(ex.ubm_echo_last_error(), CODE.STATE);
  assert.equal(bleTransition('scan.start'), CODE.STATE);
  assert.ok(lastText().startsWith('lifecycle.invalid-state|core|request-ble-transition|'));
}

// --- Init contract (PKG-02 / WEB preloading). ---
assert.equal(initWith('C-UBM.9.9.9-DRAFT'), CODE.INCOMPAT);
assert.ok(lastText().startsWith('protocol.incompatible|core|echo-init|'));
assert.equal(initWith(REV), CODE.OK);
checkOk();
assert.equal(initWith(REV), CODE.OK, 'same-revision re-init is a no-op');
assert.equal(initWith('C-UBM.0.2.0-DRAFT'), CODE.INCOMPAT,
  'identity change after init fails closed');
assert.ok(runBytes(new Uint8Array([9])).ok, 'failed re-init must not de-init');

// --- Owned byte batches. ---
assert.deepEqual([...runBytes(new Uint8Array([0, 1, 2, 250, 255])).bytes],
  [0, 1, 2, 250, 255]);
assert.ok(runBytes(new Uint8Array(0)).ok, 'empty batch echoes as length 0');
{
  const big = new Uint8Array(MAX).fill(0xab);
  const { ok, bytes: echoed } = runBytes(big);
  assert.ok(ok && echoed.length === MAX && echoed[0] === 0xab && echoed[MAX - 1] === 0xab);
}
assert.equal(ex.ubm_echo_alloc(MAX + 1), 0, 'over-cap alloc refuses (null)');
assert.equal(ex.ubm_echo_alloc(0), 0, 'zero alloc refuses (null)');

// --- DATA-02 lossless u64 counters via BigInt <-> decimal. ---
for (const n of [0n, 1n, 9007199254740993n, 9223372036854775807n, 18446744073709551615n]) {
  const r = runCounter(n.toString());
  assert.ok(r.ok, `counter ${n}`);
  assert.equal(BigInt(r.text), n, `counter ${n} lossless`);
}
assert.equal(runCounter('00042').text, '42');
for (const bad of ['', '-1', '+5', '12a34', ' 42', '4.0', '0x10', '18446744073709551616']) {
  const r = runCounter(bad);
  assert.ok(!r.ok, `counter ${JSON.stringify(bad)} must fail`);
  assert.equal(ex.ubm_echo_last_error(), CODE.BYTES_INVALID);
  assert.ok(lastText().startsWith('bytes.invalid|core|echo-counter|'), lastText());
}

// --- U7 transition-driving: the binding holds and drives a REAL Kernel+Central. ---
{
  const status = statusJson();
  assert.ok(status.ok, 'status must succeed once initialised');
  assert.equal(status.text,
    '{"revision":"C-UBM.0.1.1-DRAFT","live_operations":0,"retained_cleanup":0}');
  checkOk();
  // Real kernel expiry sweeps settle nothing on a fresh central (twice),
  // over decimal-string host time (DATA-02 mapping, lossless past 2^53).
  for (const now of ['0', '18446744073709551615']) {
    const r = sweep(now);
    assert.equal(r.code, CODE.OK, `sweep(${now})`);
    assert.equal(r.settled, 0n, `sweep(${now}) settles nothing fresh`);
  }
  checkOk();
  assert.equal(sweep('nope').code, CODE.BYTES_INVALID);
  assert.ok(lastText().startsWith('bytes.invalid|core|central-expire-sweep|'));
  // Real shutdown transition: clean release, idempotent.
  for (let i = 0; i < 2; i++) {
    const r = destroyCentral();
    assert.ok(r.ok, 'destroy must succeed');
    assert.equal(r.text, 'released', 'fresh central releases cleanly');
  }
  // No fake passes: unwired BLE transitions reject loudly with the frozen
  // capability.unsupported|capability contract pairing.
  for (const transition of ['scan.start', 'queue-advertisement', 'force-disconnect',
    'advance-time', 'emit-notification']) {
    assert.equal(bleTransition(transition), CODE.CAP, `unwired ${transition}`);
    assert.equal(lastText(),
      'capability.unsupported|capability|request-ble-transition|transition-not-wired-in-u7-slice');
  }
  assert.equal(bleTransition(''), CODE.ARG);
  assert.ok(lastText().startsWith('argument.invalid|core|request-ble-transition|'));
  assert.ok(runBytes(new Uint8Array([9])).ok, 'usable after destroy drive');
}

// --- Streaming echo + cooperative cancellation. ---
function begin() {
  const h = ex.ubm_echo_stream_begin();
  assert.notEqual(h, 0n, 'begin must succeed once initialised');
  checkOk();
  return h;
}
function push(h, chunk) {
  const ptr = writeInput(chunk);
  const total = ex.ubm_echo_stream_push(h, ptr, chunk.length);
  freeAlloc(ptr, chunk.length);
  return total;
}
function finish(h) {
  setOut(0);
  const ptr = ex.ubm_echo_stream_finish(h, scratch);
  const len = getOut();
  const out = takePublished(ptr, len);
  if (out === null) return { ok: false };
  return { ok: true, bytes: out };
}
{
  const h = begin();
  assert.equal(push(h, new Uint8Array([1, 2])), 2);
  assert.equal(push(h, new Uint8Array(0)), 2, 'empty push is a no-op success');
  checkOk();
  assert.equal(push(h, new Uint8Array([3])), 3);
  const r = finish(h);
  assert.ok(r.ok && [...r.bytes].join(',') === '1,2,3');
  checkOk();
  assert.ok(!finish(h).ok, 'second finish must fail');
  assert.equal(ex.ubm_echo_last_error(), CODE.STATE);
  assert.ok(lastText().startsWith('lifecycle.invalid-state|core|echo-stream-finish|'));
  // usize::MAX on wasm32 (u32) signals push failure.
  assert.equal(push(h, new Uint8Array([9])), -1);
  assert.equal(ex.ubm_echo_last_error(), CODE.STATE);
}

// Cancel: push-after-cancel and finish-after-cancel report aborted exactly
// once; the consumed handle then rejects loudly.
{
  const h = begin();
  push(h, new Uint8Array([5, 6]));
  assert.equal(ex.ubm_echo_stream_cancel(h), CODE.OK);
  assert.equal(push(h, new Uint8Array([7])), -1);
  assert.equal(ex.ubm_echo_last_error(), CODE.ABORTED);
  assert.ok(!finish(h).ok, 'cancelled finish must fail');
  assert.equal(ex.ubm_echo_last_error(), CODE.ABORTED);
  assert.ok(lastText().startsWith('operation.aborted|core|echo-stream-finish|'));
  // M1 analogue (no session close on this boundary): cancel on the consumed
  // handle rejects loudly, never a silent replay or second abort.
  assert.equal(ex.ubm_echo_stream_cancel(h), CODE.STATE);
  assert.ok(lastText().startsWith('lifecycle.invalid-state|core|echo-stream-cancel|'),
    lastText());
  assert.ok(!finish(h).ok, 'consumed handle must fail again');
  assert.equal(ex.ubm_echo_last_error(), CODE.STATE);
}

// Unknown and null handles reject loudly, never alias a live stream.
{
  setOut(0);
  assert.equal(ex.ubm_echo_stream_finish(999999n, scratch), 0);
  assert.equal(ex.ubm_echo_last_error(), CODE.STATE);
  const ptr = writeInput(new Uint8Array([1]));
  assert.equal(ex.ubm_echo_stream_push(999999n, ptr, 1), -1);
  freeAlloc(ptr, 1);
  assert.equal(ex.ubm_echo_last_error(), CODE.STATE);
  assert.equal(ex.ubm_echo_stream_cancel(999999n), CODE.STATE);
}

// Cap enforcement over the boundary: accumulation past MAX rejects
// bytes.too-large but keeps the stream usable.
{
  const h = begin();
  const quarter = new Uint8Array(131072).fill(0x11);
  for (let i = 0; i < 3; i++) push(h, quarter); // 393216 of 524288
  const over = new Uint8Array(131073).fill(0xff);
  const overPtr = writeInput(over);
  assert.equal(ex.ubm_echo_stream_push(h, overPtr, over.length), -1);
  freeAlloc(overPtr, over.length);
  assert.equal(ex.ubm_echo_last_error(), CODE.TOO_LARGE);
  assert.ok(lastText().startsWith('bytes.too-large|core|echo-stream-push|'));
  assert.equal(push(h, new Uint8Array([0x22])), 393217, 'stream usable after refused push');
  const r = finish(h);
  assert.ok(r.ok && r.bytes.length === 393217 && r.bytes[393216] === 0x22);
}

// Interleaved streams stay independent (synchronous reentrancy: no shared
// mutable cursor, each call completes fully before returning).
{
  const a = begin(), b = begin();
  assert.notEqual(a, b);
  push(a, new Uint8Array([1]));
  push(b, new Uint8Array([2, 3]));
  push(a, new Uint8Array([4]));
  assert.equal(ex.ubm_echo_stream_cancel(a), CODE.OK);
  assert.ok(!finish(a).ok && ex.ubm_echo_last_error() === CODE.ABORTED);
  const rb = finish(b);
  assert.ok(rb.ok && [...rb.bytes].join(',') === '2,3');
}

// JSON bridge document.
{
  setOut(0);
  const ptr = ex.ubm_echo_describe_json(scratch);
  const len = getOut();
  assert.notEqual(ptr, 0);
  const doc = JSON.parse(dec.decode(takePublished(ptr, len)));
  assert.equal(doc.revision, REV);
  assert.equal(doc.maxBytes, MAX);
  assert.equal(BigInt(doc.u64max), 18446744073709551615n);
}

// Panic probes were deleted from production paths by the wiring slice: no
// feasibility-only export may ship. The production module exposes no trap
// probe; a panic still traps the module (documented institute behaviour),
// and hosts treat a trap as session-fatal for the in-flight call.
{
  assert.equal(ex.ubm_echo_panic_probe, undefined,
    'panic probe must not exist on the production module');
  assert.ok(runBytes(new Uint8Array([1, 2])).ok, 'module serves after the probe check');
}

// Byte ownership accounting: every alloc and every published buffer freed
// exactly once (empty/null results are never freed by construction).
// Leak probe: repeat identical fixed-size ownership cycles; module memory
// must not grow (freed blocks are reused). A 4 KiB x 100 leak would add
// ~800 KiB (>= 12 pages); clean reuse adds zero bytes.
{
  const cycle = new Uint8Array(4096).fill(0x77);
  const before = ex.memory.buffer.byteLength;
  for (let i = 0; i < 100; i++) {
    const r = runBytes(cycle);
    assert.ok(r.ok && r.bytes.length === 4096 && r.bytes[0] === 0x77);
  }
  const after = ex.memory.buffer.byteLength;
  assert.equal(after, before, `module memory grew ${before} -> ${after}: leak`);
}
freeAlloc(scratch, 8);
assert.equal(liveAlloc, 0, `alloc ledger must balance, live=${liveAlloc}`);
assert.ok(ownershipCycles > 100, 'harness must exercise many ownership cycles');

console.log('wasm-roundtrip: OK');
