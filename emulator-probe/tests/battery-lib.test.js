// emulator-probe/tests/battery-lib.test.js
// Test-first harness contract for the UBM 5.0 Android-emulator slice
// (trackourhealth/bun-mono#1188, U-ANDROID-EMULATOR row).
// Run: node --test emulator-probe/tests/
'use strict';
const { describe, it } = require('node:test');
const assert = require('node:assert/strict');

const lib = require('../lib/battery-lib.js');

describe('task-owned AVD naming', () => {
  it('accepts the task-owned probe name', () => {
    assert.equal(lib.validateAvdName('ubm5-emu-probe'), 'ubm5-emu-probe');
  });
  it('rejects names outside the task-owned prefix (never touch existing AVDs)', () => {
    assert.throws(() => lib.validateAvdName('Pixel_7_API_34'), /task-owned/);
    assert.throws(() => lib.validateAvdName(''), /task-owned/);
  });
});

describe('explicit serial selection', () => {
  const devicesOutput = [
    'List of devices attached',
    'emulator-5554\tdevice product:sdk_gphone64_x86_64 model:sdk_gphone64_x86_64 device:emulator64_x86_64_arm64 transport_id:1',
    '',
  ].join('\n');
  it('parses adb devices -l output', () => {
    const devices = lib.parseAdbDevices(devicesOutput);
    assert.equal(devices.length, 1);
    assert.equal(devices[0].serial, 'emulator-5554');
    assert.equal(devices[0].state, 'device');
  });
  it('picks the wanted serial explicitly', () => {
    const devices = lib.parseAdbDevices(devicesOutput);
    assert.equal(lib.pickSerial(devices, 'emulator-5554'), 'emulator-5554');
  });
  it('fails closed on empty, ambiguous, or missing serials', () => {
    assert.throws(() => lib.pickSerial([], 'emulator-5554'), /no devices/);
    const two = [
      { serial: 'emulator-5554', state: 'device' },
      { serial: 'emulator-5556', state: 'device' },
    ];
    assert.throws(() => lib.pickSerial(two), /ambiguous/);
    assert.throws(() => lib.pickSerial(lib.parseAdbDevices(devicesOutput), 'emulator-5556'), /not found/);
  });
  it('derives the expected serial from the adb port', () => {
    assert.equal(lib.serialForAdbPort(5554), 'emulator-5554');
  });
});

describe('emulator launch profile (ABI matched to host accel)', () => {
  it('builds headless x86_64 launch args with an explicit port', () => {
    const args = lib.emulatorArgs({ avd: 'ubm5-emu-probe', adbPort: 5554 });
    assert.ok(args.includes('-no-window'));
    assert.ok(args.includes('-no-audio'));
    assert.ok(args.includes('-gpu'));
    assert.ok(args.includes('ubm5-emu-probe'));
    assert.ok(args.includes('5554'));
  });
  it('rejects non-task-owned AVDs at arg-build time', () => {
    assert.throws(() => lib.emulatorArgs({ avd: 'Pixel_7_API_34', adbPort: 5554 }), /task-owned/);
  });
});

describe('result origin labeling (fail-closed)', () => {
  it('accepts REAL-EMULATOR, SIMULATED-injection, HOST-JVM', () => {
    assert.equal(lib.labelOrigin('REAL-EMULATOR'), 'REAL-EMULATOR');
    assert.equal(lib.labelOrigin('SIMULATED-injection'), 'SIMULATED-injection');
    assert.equal(lib.labelOrigin('HOST-JVM'), 'HOST-JVM');
  });
  it('rejects virtual-radio-as-physical claims', () => {
    assert.throws(() => lib.labelOrigin('PHYSICAL'), /origin/);
    assert.throws(() => lib.labelOrigin('REAL-DEVICE'), /origin/);
    assert.throws(() => lib.labelOrigin(''), /origin/);
  });
});

describe('log redaction', () => {
  it('redacts home dirs and BT MACs but keeps emulator serials', () => {
    const raw = 'path /home/stephane/src/x device AA:BB:CC:DD:EE:FF on emulator-5554';
    const redacted = lib.redactLog(raw);
    assert.ok(!redacted.includes('/home/stephane'), 'home dir leaked');
    assert.ok(!redacted.includes('AA:BB:CC:DD:EE:FF'), 'MAC leaked');
    assert.ok(redacted.includes('emulator-5554'), 'serial should be kept');
    assert.ok(redacted.includes('$HOME'), 'home replacement marker missing');
  });
});

describe('retained support/permission matrix constants', () => {
  it('matches android/gradle.properties and the documented permission splits', () => {
    const m = lib.permissionMatrix();
    assert.equal(m.minSdk, 24);
    assert.equal(m.runtimePermissionSplitApi, 31);
    assert.equal(m.postNotificationsApi, 33);
    assert.equal(m.targetSdk, 36);
    assert.equal(m.compileSdk, 36);
  });
});

describe('uiautomator dump parsing (dumps are effectively one line)', () => {
  const dump = `<?xml version='1.0' encoding='UTF-8' standalone='yes' ?><hierarchy rotation="0"><node index="0" text="" class="android.widget.FrameLayout" bounds="[0,0][320,560]"><node index="1" text="Reload" class="android.widget.TextView" bounds="[16,200][304,248]" /></node></hierarchy>`;
  it('finds the exact-text node center, not the first bounds on the line', () => {
    assert.deepEqual(lib.parseUiDumpCenter(dump, 'Reload'), [160, 224]);
  });
  it('returns null when the text is absent', () => {
    assert.equal(lib.parseUiDumpCenter(dump, 'Reboot'), null);
  });
  it('does not substring-match a longer label', () => {
    assert.equal(lib.parseUiDumpCenter(dump, 'Reloa'), null);
  });
});

describe('battery summary', () => {
  it('totals assertions and groups by result', () => {
    const records = [
      { id: 'T1-install', assertions: { passed: 3, total: 3 }, result: 'pass', origin: 'REAL-EMULATOR' },
      { id: 'T2-lib', assertions: { passed: 1, total: 2 }, result: 'fail', origin: 'REAL-EMULATOR' },
      { id: 'T3-reload', assertions: { passed: 0, total: 0 }, result: 'boundary', origin: 'REAL-EMULATOR' },
    ];
    const summary = lib.summarizeBattery(records);
    assert.equal(summary.tests, 3);
    assert.equal(summary.assertionsPassed, 4);
    assert.equal(summary.assertionsTotal, 5);
    assert.deepEqual(summary.byResult, { pass: 1, fail: 1, boundary: 1 });
  });
  it('rejects records with unlabelled origins', () => {
    assert.throws(
      () => lib.summarizeBattery([{ id: 'Tx', assertions: { passed: 0, total: 0 }, result: 'pass', origin: 'PHYSICAL' }]),
      /origin/,
    );
  });
});
