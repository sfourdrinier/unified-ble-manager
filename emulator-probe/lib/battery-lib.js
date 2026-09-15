// emulator-probe/lib/battery-lib.js
// Pure helpers for the UBM 5.0 Android-emulator probe harness.
// No dependencies beyond Node stdlib. All device contact goes through
// explicit serials; every reported result carries a fail-closed origin
// label (REAL-EMULATOR vs SIMULATED-injection vs HOST-JVM).
'use strict';

const TASK_AVD_PREFIX = 'ubm5-emu-';
const ORIGINS = new Set(['REAL-EMULATOR', 'SIMULATED-injection', 'HOST-JVM']);

function validateAvdName(name) {
  if (typeof name !== 'string' || !name.startsWith(TASK_AVD_PREFIX) || name.length <= TASK_AVD_PREFIX.length) {
    throw new Error(`refusing non-task-owned AVD name ${JSON.stringify(name)} (must start with ${TASK_AVD_PREFIX})`);
  }
  return name;
}

function parseAdbDevices(output) {
  const lines = String(output).split('\n').slice(1);
  const devices = [];
  for (const line of lines) {
    const trimmed = line.trim();
    if (!trimmed) continue;
    const [serial, state] = trimmed.split(/\s+/);
    if (serial && state) devices.push({ serial, state });
  }
  return devices;
}

function pickSerial(devices, wanted) {
  if (!devices || devices.length === 0) throw new Error('no devices attached: refusing to run untargeted');
  if (wanted) {
    const found = devices.find((d) => d.serial === wanted);
    if (!found) throw new Error(`wanted serial ${wanted} not found among attached devices`);
    return found.serial;
  }
  if (devices.length > 1) {
    throw new Error(`ambiguous device set (${devices.map((d) => d.serial).join(',')}): pass an explicit serial`);
  }
  return devices[0].serial;
}

function serialForAdbPort(port) {
  return `emulator-${port}`;
}

function emulatorArgs({ avd, adbPort }) {
  validateAvdName(avd);
  return [
    '-avd', avd,
    '-no-window',
    '-no-audio',
    '-no-boot-anim',
    '-gpu', 'swiftshader_indirect',
    '-no-snapshot-save',
    '-port', String(adbPort),
  ];
}

function labelOrigin(origin) {
  if (!ORIGINS.has(origin)) {
    throw new Error(`refusing unlabelled origin ${JSON.stringify(origin)}: use REAL-EMULATOR, SIMULATED-injection, or HOST-JVM`);
  }
  return origin;
}

function redactLog(text) {
  return String(text)
    .replace(/\/home\/[^/\s]+/g, '$HOME')
    .replace(/\b([0-9A-Fa-f]{2}:){5}[0-9A-Fa-f]{2}\b/g, '<REDACTED-MAC>');
}

// Retained support/permission matrix, sourced from:
// - android/gradle.properties (min/target/compile SDK)
// - docs/GETTING_STARTED.md "Ask Android for runtime permission" (API 31 split)
// - docs/PLATFORMS.md "Connected-device background monitoring" (API 33 POST_NOTIFICATIONS)
function permissionMatrix() {
  return {
    minSdk: 24,
    runtimePermissionSplitApi: 31,
    postNotificationsApi: 33,
    targetSdk: 36,
    compileSdk: 36,
  };
}

// uiautomator dumps the hierarchy on effectively one line, so match per
// <node> tag (attributes never span tags) with exact text equality.
// Returns [cx, cy] or null.
function parseUiDumpCenter(xml, text) {
  const nodes = String(xml).match(/<node\b[^>]*>/g) || [];
  for (const node of nodes) {
    const t = node.match(/ text="([^"]*)"/);
    if (t && t[1] === text) {
      const m = node.match(/bounds="\[(\d+),(\d+)\]\[(\d+),(\d+)\]"/);
      if (m) {
        return [
          (parseInt(m[1], 10) + parseInt(m[3], 10)) >> 1,
          (parseInt(m[2], 10) + parseInt(m[4], 10)) >> 1,
        ];
      }
    }
  }
  return null;
}

function summarizeBattery(records) {
  const byResult = {};
  let assertionsPassed = 0;
  let assertionsTotal = 0;
  for (const record of records) {
    labelOrigin(record.origin);
    byResult[record.result] = (byResult[record.result] || 0) + 1;
    assertionsPassed += record.assertions.passed;
    assertionsTotal += record.assertions.total;
  }
  return { tests: records.length, assertionsPassed, assertionsTotal, byResult };
}

module.exports = {
  TASK_AVD_PREFIX,
  validateAvdName,
  parseAdbDevices,
  pickSerial,
  serialForAdbPort,
  emulatorArgs,
  labelOrigin,
  redactLog,
  parseUiDumpCenter,
  permissionMatrix,
  summarizeBattery,
};
