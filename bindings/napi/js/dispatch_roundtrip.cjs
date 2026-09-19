'use strict';
// F01 N-API dispatch round-trip: drives the REAL DesktopCentral op surface
// (scan/connect/discover/read/write/subscribe/timeout/dispose) through the
// packed dispatch addon and proves every op executes in the candidate Rust
// runtime with frozen contract identities. Fails loudly; no skips.
//
// Two backends: `openSynthetic` (deterministic, hardware-free: the CI leg)
// and `open` (production radio: must fail LOUDLY with adapter.unavailable
// when no adapter exists, never silently).
//
// Hardware boundary (F01 Leg F real-radio coverage):
//   * Synthetic (`openSynthetic`) is fully deterministic and runs
//     everywhere, including headless CI: scan/connect/discover/read/write/
//     subscribe/timeout/dispose below exercise the REAL DesktopCentral op
//     surface in Rust, no host Bluetooth involved.
//   * Production (`open`) needs a host adapter. Headless CI (no adapter)
//     proves the boundary from the other side: open MUST reject with the
//     frozen `adapter.unavailable|adapter` identity, never hang and never
//     return a fake central. That loud failure IS the headless assertion.
//   * Where hardware exists (workstation/device/macOS CI), open succeeds
//     and the round-trip additionally drives a bounded production
//     startScan/stopScan through the real radio before close: the same op
//     surface, the physical backend. The branch taken is printed, so a log
//     always shows which side of the boundary was proven.
const assert = require('node:assert/strict');
const path = require('node:path');

const ADDON =
  process.env.UBM_NAPI_ADDON || path.join(__dirname, '..', `ubm_echo.${process.platform}-${process.arch}.node`);
const addon = require(ADDON);

// The radio family this binary drives (UbmCentral.open refuses any other).
const PLATFORM = { linux: 'bluez', darwin: 'corebluetooth', win32: 'winrt' }[process.platform];

const HRM_SERVICE = '0000180d-0000-1000-8000-00805f9b34fb';
const HRM_MEASUREMENT = '00002a37-0000-1000-8000-00805f9b34fb';
const HRM_BODY_LOCATION = '00002a38-0000-1000-8000-00805f9b34fb';
const CHAR_USER_DESCRIPTION = '00002901-0000-1000-8000-00805f9b34fb';

function codeOf(err) {
  const m = /^([^|]+)\|([^|]+)\|([^|]+)\|/.exec(String(err && err.message));
  assert.ok(m, `error must carry code|domain|operation wire form, got: ${err && err.message}`);
  return { code: m[1], domain: m[2], operation: m[3] };
}

async function rejectsWith(promise, code, domain, label) {
  await assert.rejects(
    promise,
    err => {
      const got = codeOf(err);
      return got.code === code && got.domain === domain;
    },
    `${label}: expected rejection ${code}|${domain}`
  );
}

async function pollFor(take, label) {
  const deadline = Date.now() + 5000;
  for (;;) {
    const value = await take();
    if (value !== null && value !== undefined) return value;
    assert.ok(Date.now() < deadline, `${label}: radio forwarding never delivered`);
    await new Promise(resolve => setTimeout(resolve, 10));
  }
}

function selector() {
  return {
    serviceUuid: HRM_SERVICE,
    serviceOccurrence: 0,
    characteristicUuid: HRM_MEASUREMENT,
    characteristicOccurrence: 0
  };
}

function hrmServices() {
  return [
    {
      uuid: HRM_SERVICE,
      occurrence: 0,
      characteristics: [
        {
          uuid: HRM_MEASUREMENT,
          occurrence: 0,
          properties: {
            read: true,
            write: false,
            writeWithoutResponse: false,
            notify: true,
            indicate: false
          },
          descriptors: [{ uuid: CHAR_USER_DESCRIPTION, occurrence: 0 }]
        },
        {
          uuid: HRM_BODY_LOCATION,
          occurrence: 0,
          properties: {
            read: true,
            write: true,
            writeWithoutResponse: true,
            notify: false,
            indicate: false
          },
          descriptors: []
        }
      ]
    }
  ];
}

async function main() {
  assert.equal(typeof addon.UbmCentral, 'function', 'addon exports the UbmCentral dispatch class');

  // Owner admission fails closed, both backends.
  await rejectsWith(addon.UbmCentral.openSynthetic(''), 'argument.invalid', 'core', 'synthetic empty owner');
  await rejectsWith(addon.UbmCentral.open({ owner: '', platform: PLATFORM }), 'argument.invalid', 'core', 'production empty owner');

  const central = await addon.UbmCentral.openSynthetic('dispatch-roundtrip');

  // Scan + full F22 observation metadata through the real queue.
  const session = await central.startScan({ owner: 'dispatch-roundtrip', serviceUuids: [], timeoutMs: 5000 });
  assert.ok(session && typeof session.operationId === 'string' && session.operationId.length > 0, 'scan session carries an op id');
  await central.stageAdvertisement({
    peerId: 'peer-1',
    rssi: -60,
    localName: 'Movesense',
    serviceUuids: [HRM_SERVICE],
    manufacturerData: [{ companyId: 107, payload: Buffer.from([0x02, 0x15]) }],
    serviceData: [{ uuid: HRM_SERVICE, payload: Buffer.from([0x06, 0x40]) }],
    txPower: -4
  });
  const obs = await pollFor(() => central.takeAdvertisement(), 'takeAdvertisement');
  assert.equal(obs.peerId, 'peer-1');
  assert.equal(obs.rssi, -60);
  assert.equal(obs.localName, 'Movesense');
  assert.deepEqual(obs.serviceUuids, [HRM_SERVICE]);
  assert.equal(obs.manufacturerData.length, 1);
  assert.equal(obs.manufacturerData[0].companyId, 107);
  assert.ok(Buffer.isBuffer(obs.manufacturerData[0].payload), 'manufacturer payload is a Buffer');
  assert.deepEqual([...obs.manufacturerData[0].payload], [0x02, 0x15]);
  assert.equal(obs.serviceData.length, 1);
  assert.equal(obs.serviceData[0].uuid, HRM_SERVICE);
  assert.deepEqual([...obs.serviceData[0].payload], [0x06, 0x40]);
  assert.equal(obs.txPower, -4);
  assert.equal(await central.stopScan(session.operationId), 'stopped', 'stop names its own scan');
  assert.equal(await central.stopScan(session.operationId), 'not-active', 'a stopped scan id is no longer active');

  // Connect + full discovery tree.
  const handle = await central.connect({ peerId: 'peer-1', lease: 'lease-a', timeoutMs: 5000 });
  assert.ok(handle && typeof handle.peerKey === 'string' && handle.peerKey.length > 0, 'connect returns a peer key');
  await central.stageServices('peer-1', hrmServices());
  const report = await central.discover({ peerId: 'peer-1', lease: 'lease-a' });
  assert.deepEqual(report, { pathsRegistered: 4 }, 'discovery registers whole; nothing is skipped');
  const paths = await central.discoveredPaths('peer-1');
  assert.equal(paths.length, 4);
  assert.deepEqual(
    paths.map(p => [
      p.serviceUuid,
      p.serviceOccurrence,
      // napi renders Rust None as undefined.
      p.characteristicUuid ?? null,
      p.descriptorUuid ?? null
    ]),
    [
      [HRM_SERVICE, 0, null, null],
      [HRM_SERVICE, 0, HRM_MEASUREMENT, null],
      [HRM_SERVICE, 0, HRM_MEASUREMENT, CHAR_USER_DESCRIPTION],
      [HRM_SERVICE, 0, HRM_BODY_LOCATION, null]
    ]
  );
  assert.equal(paths[1].properties, 0x09, 'read+notify property bits survive');

  // Read (unstaged synthetic default), descriptor read, write.
  const read = await central.read({ peerId: 'peer-1', selector: selector(), timeoutMs: 5000 });
  assert.ok(Buffer.isBuffer(read.value), 'read returns its value as a Buffer');
  assert.deepEqual([...read.value], [0x42]);
  assert.equal(read.provenance, 'read-response', 'the synthetic radio attributes its reads');
  const descValue = await central.readDescriptor({
    peerId: 'peer-1',
    selector: { ...selector(), descriptorUuid: CHAR_USER_DESCRIPTION, descriptorOccurrence: 0 },
    timeoutMs: 5000
  });
  assert.deepEqual([...descValue], [0x01]);
  // Writes need a measured MTU. The backend does not pre-check the write
  // property (finding 83): as the legacy backends did, the OS answers —
  // the public GATT layer (`src/public/gatt.ts` resolveWriteMode) is where
  // a missing write property is refused before any backend call. So a
  // write to the notify-only measurement reaches the radio, and the
  // radio's refusal is the result, carried verbatim.
  await central.stageMtu('peer-1', 128);
  const writesBefore = (await central.stagedRadioCalls()).filter(call => call === 'write_characteristic').length;
  await central.failNextRadioOpWithPlatform('write', 'write not permitted', {
    domain: 'synthetic-os',
    code: 'write-not-permitted'
  });
  await assert.rejects(
    central.write({
      peerId: 'peer-1',
      selector: selector(),
      value: Buffer.from([0x01]),
      mode: 'without-response',
      timeoutMs: 5000
    }),
    err => {
      const got = codeOf(err);
      return (
        got.code === 'gatt.write-failed' &&
        got.domain === 'gatt' &&
        String(err.message).includes('write not permitted')
      );
    },
    'write to notify-only char: the radio refusal is the result'
  );
  const writesAfter = (await central.stagedRadioCalls()).filter(call => call === 'write_characteristic').length;
  assert.equal(writesAfter, writesBefore + 1, 'write to notify-only char reached the radio (no core pre-check)');
  await central.write({
    peerId: 'peer-1',
    selector: { ...selector(), characteristicUuid: HRM_BODY_LOCATION },
    value: Buffer.from([0x01]),
    mode: 'without-response',
    timeoutMs: 5000
  });

  // Subscribe + notification + unsubscribe.
  await central.subscribe({ peerId: 'peer-1', selector: selector(), consumer: 'app', timeoutMs: 5000 });
  await central.stageNotification({
    peerId: 'peer-1',
    serviceUuid: HRM_SERVICE,
    serviceOccurrence: 0,
    characteristicUuid: HRM_MEASUREMENT,
    characteristicOccurrence: 0,
    value: Buffer.from([0x06, 0x40])
  });
  const note = await pollFor(
    () => central.takeNotification({ peerId: 'peer-1', selector: selector(), consumer: 'app' }),
    'takeNotification'
  );
  assert.ok(Buffer.isBuffer(note), 'notification is a Buffer');
  assert.deepEqual([...note], [0x06, 0x40]);
  const disabled = await central.unsubscribe({ peerId: 'peer-1', selector: selector(), consumer: 'app' });
  assert.equal(disabled, true, 'last consumer disables the physical CCCD');

  // Timeout owns the caller outcome: a parked radio connect whose deadline
  // expires before any link settles connection.failed with the core's
  // deadline-expired fact (finding 161), never hangs, never returns late
  // success.
  await central.stageAdvertisement({ peerId: 'peer-2', rssi: -70 });
  await central.blockRadioOp('connect');
  await assert.rejects(
    central.connect({ peerId: 'peer-2', lease: 'lease-a', timeoutMs: 300 }),
    err => {
      const got = codeOf(err);
      return (
        got.code === 'connection.failed' &&
        got.domain === 'connection' &&
        /"domain":"core","code":"deadline-expired"/.test(String(err.message))
      );
    },
    'blocked connect: expected connection.failed|connection with core/deadline-expired'
  );
  await central.unblockRadioOp('connect');

  // A never-connected peer fails reads with the frozen identity, never
  // an empty surprise (connect itself registers the peer by design).
  await rejectsWith(
    central.discoveredPaths('peer-unknown'),
    'peer.not-found',
    'connection',
    'unknown peer paths'
  );

  // Dispose: shutdown is idempotent; later ops refuse loudly.
  await central.disconnect({ peerId: 'peer-1', lease: 'lease-a' });
  await central.close();
  await central.close();
  await rejectsWith(central.startScan({ owner: 'x', timeoutMs: 100 }), 'adapter.unavailable', 'adapter', 'post-close scan');

  // Production radio without hardware fails loudly (or, on a machine with
  // Bluetooth, opens and scans bounded through the real radio — never a
  // silent third outcome).
  try {
    const prod = await addon.UbmCentral.open({ owner: 'dispatch-roundtrip-prod', platform: PLATFORM });
    try {
      const prodSession = await prod.startScan({ owner: 'dispatch-roundtrip-prod', serviceUuids: [], timeoutMs: 2000 });
      assert.ok(
        prodSession && typeof prodSession.operationId === 'string' && prodSession.operationId.length > 0,
        'production scan session carries an op id'
      );
      await prod.stopScan(prodSession.operationId);
    } finally {
      await prod.close();
    }
    console.log('dispatch-roundtrip: production radio present; open+scan+close clean');
  } catch (err) {
    const got = codeOf(err);
    assert.equal(got.code, 'adapter.unavailable', `production open without hardware: ${err && err.message}`);
    assert.equal(got.domain, 'adapter', `production refusal domain: ${err && err.message}`);
    console.log('dispatch-roundtrip: production radio absent; loud adapter.unavailable');
  }

  console.log('dispatch-roundtrip: PASS (synthetic scan/connect/discover/read/write/subscribe/timeout/dispose in Rust)');
}

main().catch(err => {
  console.error(`dispatch-roundtrip FAIL: ${err && err.stack ? err.stack : err}`);
  process.exit(1);
});
