// emulator-probe/scripts/run-battery.js
// UBM 5.0 Android-emulator app-running battery (U-ANDROID-EMULATOR row).
// Drives ONLY the explicit --serial target. Every adb call carries its own
// timeout; a timeout records HUNG-SKIPPED once with the exact command and is
// never retried blindly.
//
// Robustness rule learned on the lane: device-side pipes
// (`logcat -d | grep ...`) flake on this emulator profile, so device commands
// contain no pipes -- full outputs are saved under results/logs/ and matched
// on the host. Raw evidence is redacted (home dirs, BT MACs).
//
// Usage:
//   node emulator-probe/scripts/run-battery.js --serial emulator-5554 \
//     --apk example/android/app/build/outputs/apk/debug/app-debug.apk \
//     [--only T1,T6] [--attempt-metro]
'use strict';
const { spawnSync } = require('node:child_process');
const fs = require('node:fs');
const http = require('node:http');
const path = require('node:path');
const lib = require('../lib/battery-lib.js');

const REPO_ROOT = path.resolve(__dirname, '..', '..');
const RESULTS_DIR = path.join(REPO_ROOT, 'emulator-probe', 'results');
const LOGS_DIR = path.join(RESULTS_DIR, 'logs');
let PKG = 'com.bleplxexample';
let ACTIVITY = `${PKG}/.MainActivity`;
const NATIVE_LIB = 'libunified_ble_native_protocol';

function parseArgs(argv) {
  const out = { only: null, attemptMetro: false };
  for (let i = 0; i < argv.length; i++) {
    if (argv[i] === '--serial') out.serial = argv[++i];
    else if (argv[i] === '--apk') out.apk = argv[++i];
    else if (argv[i] === '--only') out.only = argv[++i].split(',');
    else if (argv[i] === '--attempt-metro') out.attemptMetro = true;
    else if (argv[i] === '--package') out.pkg = argv[++i];
    else if (argv[i] === '--activity') out.activity = argv[++i];
    else if (argv[i] === '--out') out.outFile = argv[++i];
  }
  if (!out.serial) throw new Error('--serial <emulator-XXXX> is required (never untargeted)');
  if (!/^emulator-\d+$/.test(out.serial)) throw new Error(`refusing non-emulator serial ${out.serial}`);
  if (!out.apk) throw new Error('--apk <path-to-apk> is required');
  return out;
}

function rawAdbToResult(serial, args, opts) {
  const r = rawAdb(serial, args, opts);
  return { exit: r.exit, timedOut: r.timedOut, output: lib.redactLog(r.raw).slice(0, 2000) };
}

function rawAdb(serial, args, { timeoutMs = 30000 } = {}) {
  const res = spawnSync('adb', ['-s', serial, ...args], { encoding: 'utf8', timeout: timeoutMs });
  const raw = `${res.stdout || ''}\n${res.stderr || ''}`;
  return { exit: res.status, timedOut: !!(res.error && res.error.code === 'ETIMEDOUT'), raw };
}

// Device shell command with NO pipes (host does the matching).
function sh(serial, cmd, opts) {
  if (cmd.includes('|')) throw new Error(`device-side pipe refused: ${cmd}`);
  const r = rawAdb(serial, ['shell', cmd], opts);
  return { exit: r.exit, timedOut: r.timedOut, output: lib.redactLog(r.raw).slice(0, 200000) };
}

function sleepMs(ms) {
  spawnSync('sleep', [(ms / 1000).toString()]);
}

function grepLines(text, re, max) {
  const out = [];
  for (const line of String(text).split('\n')) {
    if (re.test(line)) {
      out.push(line.slice(0, 300));
      if (out.length >= (max || 10)) break;
    }
  }
  return out;
}

function check(records, id, name, origin, fn) {
  const record = { id, name, origin: lib.labelOrigin(origin), assertions: { passed: 0, total: 0 }, checks: [], evidence: [], result: 'pass' };
  fs.mkdirSync(LOGS_DIR, { recursive: true });
  const ctx = {
    note: (checkName, pass, detail) => {
      record.assertions.total += 1;
      if (pass) record.assertions.passed += 1;
      else record.result = 'fail';
      record.checks.push({ check: checkName, pass: !!pass, detail: String(detail || '').slice(0, 300) });
    },
    snap: (label, result) => {
      const file = path.join(LOGS_DIR, `${id}-${record.evidence.length}.log`);
      fs.writeFileSync(file, `$ ${label}\n[exit=${result.exit}${result.timedOut ? ' TIMED-OUT' : ''}]\n${result.output}\n`);
      record.evidence.push({ command: label, exit: result.exit, timedOut: !!result.timedOut, log: path.relative(RESULTS_DIR, file) });
      return result;
    },
    hung: (command) => {
      record.result = 'skipped';
      record.hung = true;
      record.checks.push({ check: 'completed without hanging', pass: false, detail: `HUNG-SKIPPED: ${command}` });
    },
    boundary: (reason) => {
      if (record.result === 'pass' && record.assertions.total === 0) record.result = 'boundary';
      record.boundaryReason = reason;
    },
  };
  try {
    fn(ctx);
  } catch (err) {
    record.result = 'fail';
    record.checks.push({ check: 'threw', pass: false, detail: String((err && err.message) || err).slice(0, 300) });
  }
  records.push(record);
  return record;
}

function pidof(serial, ctx, tag) {
  const r = ctx.snap(tag, sh(serial, `pidof ${PKG}`, { timeoutMs: 30000 }));
  return (r.output.trim().split(/\s+/)[0] || '').replace(/\D/g, '');
}

function logcatDump(serial, ctx, tag) {
  return ctx.snap(tag, sh(serial, 'logcat -d -v brief', { timeoutMs: 60000 })).output;
}

// Probe-consumer button centers on AVD ubm5-emu-probe (uiautomator device
// pixels; re-derive with scripts if the layout or AVD changes).
const PROBE_TAP = {
  init: [160, 97],
  scan: [160, 140],
  bonded: [160, 183],
  aborted: [160, 226],
};

function tap(serial, x, y) {
  return sh(serial, `input tap ${x} ${y}`, { timeoutMs: 30000 });
}

// Finds an exact-text UI node via uiautomator and taps its center.
// Returns the tapped center or null when absent.
function tapText(serial, ctx, id, text) {
  sh(serial, 'uiautomator dump /sdcard/window_dump.xml', { timeoutMs: 60000 });
  const pulled = rawAdb(serial, ['pull', '/sdcard/window_dump.xml', `${LOGS_DIR}/${id}-ui.xml`], { timeoutMs: 60000 });
  if (pulled.exit !== 0) return null;
  const xml = fs.readFileSync(`${LOGS_DIR}/${id}-ui.xml`, 'utf8');
  const center = lib.parseUiDumpCenter(xml, text);
  if (center) {
    tap(serial, center[0], center[1]);
    return center;
  }
  return null;
}

function metroUp() {
  return new Promise((resolve) => {
    const req = http.get('http://127.0.0.1:8081/status', { timeout: 8000 }, (res) => {
      let body = '';
      res.on('data', (c) => { body += c; });
      res.on('end', () => resolve(/packager-status:running/.test(body)));
    });
    req.on('error', () => resolve(false));
    req.on('timeout', () => { req.destroy(); resolve(false); });
  });
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  if (args.pkg) PKG = args.pkg;
  ACTIVITY = args.activity || `${PKG}/.MainActivity`;
  const only = args.only ? new Set(args.only) : null;
  const want = (id) => !only || only.has(id);
  const apkPath = path.isAbsolute(args.apk) ? args.apk : path.join(REPO_ROOT, args.apk);
  const records = [];

  if (want('PRE')) check(records, 'PRE', 'device identity + preconditions', 'REAL-EMULATOR', (ctx) => {
    ctx.snap('adb uninstall (idempotent reruns)', rawAdbToResult(args.serial, ['uninstall', PKG], { timeoutMs: 60000 }));
    const props = ctx.snap('adb shell getprop fingerprint/sdk/abi/release', sh(args.serial, 'getprop ro.build.fingerprint'));
    const sdk = sh(args.serial, 'getprop ro.build.version.sdk');
    const abi = sh(args.serial, 'getprop ro.product.cpu.abi');
    ctx.note('device reachable (sdk_gphone64_x86_64)', props.exit === 0 && /sdk_gphone64_x86_64/.test(props.output), `${props.output.trim()} sdk=${sdk.output.trim()} abi=${abi.output.trim()}`);
    const bt = ctx.snap('adb shell dumpsys bluetooth_manager', sh(args.serial, 'dumpsys bluetooth_manager', { timeoutMs: 30000 }));
    ctx.note('bluetooth_manager queryable', bt.exit === 0 && /Bluetooth Status/.test(bt.output), grepLines(bt.output, /enabled:|state:|address:/, 3).join(' '));
    const absent = ctx.snap('pre-install package check', sh(args.serial, 'pm list packages'));
    ctx.note('package absent before install', !new RegExp(PKG).test(absent.output), 'pm list packages scanned on host');
  });

  if (want('T1')) check(records, 'T1', 'install UBM consumer APK (real 4.x native module path)', 'REAL-EMULATOR', (ctx) => {
    const r = rawAdb(args.serial, ['install', '-r', apkPath], { timeoutMs: 120000 });
    const inst = { exit: r.exit, timedOut: r.timedOut, output: lib.redactLog(r.raw).slice(0, 2000) };
    ctx.snap(`adb install -r ${path.basename(apkPath)}`, inst);
    if (inst.timedOut) return ctx.hung(`adb -s ${args.serial} install -r ${apkPath}`);
    ctx.note('install reports Success', inst.exit === 0 && /Success/.test(inst.output), grepLines(inst.output, /Success|Failure/, 2).join(' '));
    const listed = ctx.snap('pm list packages (host-matched)', sh(args.serial, 'pm list packages'));
    ctx.note('package listed after install', new RegExp(PKG).test(listed.output), `package:${PKG}`);
    const info = ctx.snap('dumpsys package (host-matched)', sh(args.serial, `dumpsys package ${PKG}`, { timeoutMs: 30000 }));
    const ver = grepLines(info.output, /versionCode=|minSdk=|targetSdk=/, 3).join(' ');
    ctx.note('package info queryable (v1/min24/target36)', /versionCode=1/.test(info.output) && /minSdk=24/.test(info.output) && /targetSdk=36/.test(info.output), ver);
  });

  if (want('T2')) check(records, 'T2', 'native library load surface (x86_64 ABI matched to host KVM)', 'REAL-EMULATOR', (ctx) => {
    // extractNativeLibs=false: .so files mmap-load from the APK at first use.
    // NOTE (emu-review fix): no /proc/<pid>/maps capture is committed, so a
    // named-.so dlopen is NOT directly proven here. Native load stands on the
    // SoLoader DirectApkSoSource[.../base.apk!/lib/x86_64] lines plus the
    // TurboModule-bridge round-trips in T8 (manager-created, bonded-count=0,
    // typed BleErrors) with no UnsatisfiedLinkError. A correct-PID maps
    // capture (run-as <pkg> cat /proc/<probe-pid>/maps) is a battery-v2
    // follow-up; the stale T8-11 maps attempt was deleted, not evidenced.
    const info = ctx.snap('dumpsys package (host-matched ABI lines)', sh(args.serial, `dumpsys package ${PKG}`, { timeoutMs: 30000 }));
    if (info.timedOut) return ctx.hung(`adb -s ${args.serial} shell dumpsys package ${PKG}`);
    ctx.note('primary ABI is x86_64', /primaryCpuAbi=x86_64/.test(info.output), grepLines(info.output, /CpuAbi/, 3).join(' '));
    ctx.note('nativeLibraryDir declared', /legacyNativeLibraryDir=|nativeLibraryDir=/.test(info.output), grepLines(info.output, /NativeLibraryDir=/, 1).join(' '));
    ctx.note('extractNativeLibs=false (mmap-from-APK loading)', /extractNativeLibs=false/.test(info.output), 'see log');
    const dataDir = ctx.snap('run-as pwd (debuggable access)', sh(args.serial, `run-as ${PKG} pwd`));
    ctx.note('app data dir accessible via run-as', dataDir.exit === 0 && new RegExp(PKG).test(dataDir.output), dataDir.output.trim().split('\n')[0]);
  });

  if (want('T3')) check(records, 'T3', 'binding identity: launch, resumed, no crash, no link errors', 'REAL-EMULATOR', (ctx) => {
    sh(args.serial, 'logcat -c', { timeoutMs: 20000 });
    const start = ctx.snap(`am start ${ACTIVITY}`, sh(args.serial, `am start -n ${ACTIVITY}`, { timeoutMs: 30000 }));
    if (start.timedOut) return ctx.hung(`adb -s ${args.serial} shell am start -n ${ACTIVITY}`);
    ctx.note('start accepted', /Starting: Intent/.test(start.output), start.output.trim().split('\n')[0]);
    sleepMs(8000);
    const pid = pidof(args.serial, ctx, 'pidof after launch');
    ctx.note('process running after launch', /^\d+$/.test(pid), `pid=${pid}`);
    const log = logcatDump(args.serial, ctx, 'logcat after launch');
    const fatal = grepLines(log, /FATAL EXCEPTION/, 4);
    const crashed = new RegExp(`Process: ${PKG}`).test(log);
    ctx.note('no FATAL EXCEPTION for consumer', !crashed, fatal.join(' ').slice(0, 200) || 'clean');
    const acts = ctx.snap('dumpsys activity activities', sh(args.serial, 'dumpsys activity activities', { timeoutMs: 60000 }));
    const resumed = grepLines(acts.output, /topResumedActivity=|mResumedActivity:?/, 2).join(' ');
    ctx.note('example activity resumed', new RegExp(PKG).test(resumed), resumed.slice(0, 200));
    ctx.note('no UnsatisfiedLinkError', !/UnsatisfiedLinkError/.test(log), 'host-matched over full logcat');
  });

  if (want('T4')) check(records, 'T4', 'setup-teardown: force-stop clears process and UBM service', 'REAL-EMULATOR', (ctx) => {
    sh(args.serial, `am start -n ${ACTIVITY}`, { timeoutMs: 30000 });
    sleepMs(6000);
    const stop = ctx.snap('am force-stop', sh(args.serial, `am force-stop ${PKG}`));
    ctx.note('force-stop exits 0', stop.exit === 0, 'ok');
    sleepMs(2000);
    const gone = pidof(args.serial, ctx, 'pidof after force-stop');
    ctx.note('process gone after teardown', gone === '', `pidof='${gone}'`);
    const svc = ctx.snap('dumpsys activity services', sh(args.serial, 'dumpsys activity services', { timeoutMs: 60000 }));
    ctx.note('no UBM service lingering', !/bleplx/i.test(svc.output), 'host-matched over full dump');
  });

  if (want('T5')) check(records, 'T5', 'foreground-background: home keeps process, relaunch resumes', 'REAL-EMULATOR', (ctx) => {
    sh(args.serial, `am start -n ${ACTIVITY}`, { timeoutMs: 30000 });
    sleepMs(6000);
    const pidBefore = pidof(args.serial, ctx, 'baseline pidof');
    ctx.note('baseline PID captured', /^\d+$/.test(pidBefore), `pid=${pidBefore}`);
    const home = ctx.snap('input KEYCODE_HOME (background)', sh(args.serial, 'input keyevent KEYCODE_HOME'));
    ctx.note('home key exits 0', home.exit === 0, 'ok');
    sleepMs(2000);
    const bg = pidof(args.serial, ctx, 'pidof while backgrounded');
    ctx.note('process survives backgrounding', bg === pidBefore && /^\d+$/.test(bg), `pid=${bg}`);
    const acts = ctx.snap('dumpsys activities while backgrounded', sh(args.serial, 'dumpsys activity activities', { timeoutMs: 60000 }));
    const resumed = grepLines(acts.output, /topResumedActivity=|mResumedActivity:?/, 2).join(' ');
    ctx.note('example activity no longer resumed', resumed !== '' && !new RegExp(PKG).test(resumed), resumed.slice(0, 160));
    ctx.snap('relaunch to foreground', sh(args.serial, `am start -n ${ACTIVITY}`, { timeoutMs: 30000 }));
    sleepMs(4000);
    const acts2 = ctx.snap('dumpsys activities after relaunch', sh(args.serial, 'dumpsys activity activities', { timeoutMs: 60000 }));
    ctx.note('relaunch resumes example activity', new RegExp(PKG).test(grepLines(acts2.output, /topResumedActivity=|mResumedActivity:?/, 2).join(' ')), 'see log');
  });

  if (want('T6')) check(records, 'T6', 'denied-revoked permissions: revoke kills process, denial surfaces as permission.denied, grant restores', 'REAL-EMULATOR', (ctx) => {
    sh(args.serial, `am force-stop ${PKG}`, { timeoutMs: 30000 });
    sleepMs(2000);
    ctx.snap('pm revoke BLUETOOTH_SCAN', sh(args.serial, `pm revoke ${PKG} android.permission.BLUETOOTH_SCAN`));
    ctx.snap('pm revoke BLUETOOTH_CONNECT', sh(args.serial, `pm revoke ${PKG} android.permission.BLUETOOTH_CONNECT`));
    const denied = ctx.snap('dumpsys package after revoke', sh(args.serial, `dumpsys package ${PKG}`, { timeoutMs: 30000 }));
    const scanLine = grepLines(denied.output, /BLUETOOTH_SCAN/, 2).join(' ');
    const connLine = grepLines(denied.output, /BLUETOOTH_CONNECT/, 2).join(' ');
    ctx.note('PackageManager reports SCAN not granted', /granted=false/.test(scanLine), scanLine.slice(0, 200));
    ctx.note('PackageManager reports CONNECT not granted', /granted=false/.test(connLine), connLine.slice(0, 200));
    // Relaunch first so the denial ping runs in a live process: revocation
    // itself kills the app (platform-owned teardown, asserted in T6b below).
    sh(args.serial, 'logcat -c', { timeoutMs: 20000 });
    ctx.snap('launch after revoke', sh(args.serial, `am start -n ${ACTIVITY}`, { timeoutMs: 30000 }));
    sleepMs(35000);
    const pid = pidof(args.serial, ctx, 'pidof after revoke-launch');
    ctx.note('app relaunches after revocation kill (no crash loop)', /^\d+$/.test(pid), `pid=${pid}`);
    tap(args.serial, ...PROBE_TAP.bonded);
    sleepMs(14000);
    const log = logcatDump(args.serial, ctx, 'logcat denial ping');
    const denial = grepLines(log, /bonded-(count|error)[^\n]*/, 2).join(' ');
    ctx.note('native boundary reports permission.denied (never empty)', /bonded-error[^]*permission\.denied[^]*enumerateBondedPeers/.test(denial), denial.slice(0, 240));
    ctx.snap('pm grant BLUETOOTH_SCAN', sh(args.serial, `pm grant ${PKG} android.permission.BLUETOOTH_SCAN`));
    ctx.snap('pm grant BLUETOOTH_CONNECT', sh(args.serial, `pm grant ${PKG} android.permission.BLUETOOTH_CONNECT`));
    const granted = ctx.snap('dumpsys package after grant', sh(args.serial, `dumpsys package ${PKG}`, { timeoutMs: 30000 }));
    ctx.note('PackageManager reports SCAN granted after restore', /granted=true/.test(grepLines(granted.output, /BLUETOOTH_SCAN/, 2).join(' ')), 'see log');
    ctx.note('PackageManager reports CONNECT granted after restore', /granted=true/.test(grepLines(granted.output, /BLUETOOTH_CONNECT/, 2).join(' ')), 'see log');
    sh(args.serial, `am force-stop ${PKG}`, { timeoutMs: 30000 });
    sleepMs(2000);
    sh(args.serial, 'logcat -c', { timeoutMs: 20000 });
    ctx.snap('launch after grant', sh(args.serial, `am start -n ${ACTIVITY}`, { timeoutMs: 30000 }));
    sleepMs(35000);
    tap(args.serial, ...PROBE_TAP.bonded);
    sleepMs(14000);
    const log2 = logcatDump(args.serial, ctx, 'logcat restored ping');
    ctx.note('bonded ping succeeds after grant (bonded-count=0)', /bonded-count=0/.test(log2), grepLines(log2, /bonded-(count|error)/, 2).join(' ').slice(0, 200));
  });

  if (want('T6b')) check(records, 'T6b', 'revocation kills the app process (platform-owned teardown)', 'REAL-EMULATOR', (ctx) => {
    sh(args.serial, `am start -n ${ACTIVITY}`, { timeoutMs: 30000 });
    sleepMs(35000);
    const before = pidof(args.serial, ctx, 'pidof before revoke');
    ctx.note('live process before revoke', /^\d+$/.test(before), `pid=${before}`);
    ctx.snap('pm revoke BLUETOOTH_CONNECT (kill probe)', sh(args.serial, `pm revoke ${PKG} android.permission.BLUETOOTH_CONNECT`));
    sleepMs(3000);
    const after = pidof(args.serial, ctx, 'pidof after revoke');
    ctx.note('process gone after revoke (platform kills on revocation)', after === '', `pidof='${after}'`);
    ctx.snap('pm grant BLUETOOTH_CONNECT (restore)', sh(args.serial, `pm grant ${PKG} android.permission.BLUETOOTH_CONNECT`));
    ctx.snap('relaunch after revoke-kill', sh(args.serial, `am start -n ${ACTIVITY}`, { timeoutMs: 30000 }));
    sleepMs(35000);
    const relaunched = pidof(args.serial, ctx, 'pidof after relaunch');
    ctx.note('app relaunches cleanly after revoke-kill', /^\d+$/.test(relaunched) && relaunched !== before, `${before} -> ${relaunched}`);
  });

  if (want('T7')) check(records, 'T7', 'process kill-relaunch: SIGKILL own PID, fresh PID on relaunch', 'REAL-EMULATOR', (ctx) => {
    sh(args.serial, `am start -n ${ACTIVITY}`, { timeoutMs: 30000 });
    sleepMs(6000);
    const pidBefore = pidof(args.serial, ctx, 'baseline pidof');
    ctx.note('baseline PID captured', /^\d+$/.test(pidBefore), `pid=${pidBefore}`);
    const kill = ctx.snap(`run-as kill -9 ${pidBefore}`, sh(args.serial, `run-as ${PKG} kill -9 ${pidBefore}`));
    ctx.note('SIGKILL own process exits 0', kill.exit === 0, 'ok');
    sleepMs(2000);
    const gone = pidof(args.serial, ctx, 'pidof after kill');
    ctx.note('old PID gone', gone === '', `pidof='${gone}'`);
    ctx.snap('relaunch after kill', sh(args.serial, `am start -n ${ACTIVITY}`, { timeoutMs: 30000 }));
    sleepMs(6000);
    const pidAfter = pidof(args.serial, ctx, 'pidof after relaunch');
    ctx.note('new PID differs (fresh process, no immortal handles)', /^\d+$/.test(pidAfter) && pidAfter !== pidBefore, `${pidBefore} -> ${pidAfter}`);
  });

  if (want('T9')) check(records, 'T9', 'service-execution loss: external foreground-service start refused', 'REAL-EMULATOR', (ctx) => {
    const svcComponent = `${PKG}/com.sfourdrinier.unifiedblemanager.BlePlxForegroundService`;
    const attempt = ctx.snap('am start-foreground-service (external)', sh(args.serial, `am start-foreground-service -n ${svcComponent}`, { timeoutMs: 30000 }));
    ctx.note('external service start refused (undeclared in bare consumer)', /SecurityException|Permission Denial|not exported|FAILED|not found|no service started/i.test(attempt.output), attempt.output.trim().split('\n').slice(0, 4).join(' ').slice(0, 220));
    ctx.boundary('In-app lease acquire/release needs JS driving (probe UI via Metro); external injection is refused because the bare consumer manifest does not declare the plugin-managed service. Behavior under an active lease remains a U-ANDROID-PHYSICAL follow-up.');
  });

  if (want('T10')) check(records, 'T10', 'error projection: adapter disable/enable round-trip, no crash', 'REAL-EMULATOR', (ctx) => {
    sh(args.serial, `am start -n ${ACTIVITY}`, { timeoutMs: 30000 });
    sleepMs(5000);
    const off = ctx.snap('svc bluetooth disable', sh(args.serial, 'svc bluetooth disable', { timeoutMs: 30000 }));
    if (off.timedOut) return ctx.hung(`adb -s ${args.serial} shell svc bluetooth disable`);
    ctx.note('disable command accepted', off.exit === 0, 'ok');
    sleepMs(3000);
    const state = ctx.snap('dumpsys bluetooth_manager after disable', sh(args.serial, 'dumpsys bluetooth_manager', { timeoutMs: 30000 }));
    ctx.note('adapter loss observable', /Bluetooth Status/.test(state.output), grepLines(state.output, /enabled:|state:/, 2).join(' '));
    const alive = pidof(args.serial, ctx, 'pidof after adapter loss');
    ctx.note('app survives adapter loss (no crash)', alive !== '', `pid=${alive}`);
    const on = ctx.snap('svc bluetooth enable (restore)', sh(args.serial, 'svc bluetooth enable', { timeoutMs: 30000 }));
    ctx.note('enable command accepted', on.exit === 0, 'ok');
  });

  if (want('T8')) {
    check(records, 'T8', 'JS-driven native battery: init/adapters/bonded/abort/scan through the real module', 'REAL-EMULATOR', (ctx) => {
      if (!args.attemptMetro) {
        ctx.boundary('Needs the probe Metro (`npx react-native start --port 8081` in emulator-probe/consumer + `adb reverse tcp:8081 tcp:8081`); rerun with --attempt-metro. Dev APK has no embedded bundle.');
        return;
      }
      ctx.note('metro packager reachable [HOST-JVM host-side]', metroUpSync(), 'host-side curl http://127.0.0.1:8081/status (not device-observed)');
      sh(args.serial, `am force-stop ${PKG}`, { timeoutMs: 30000 });
      sleepMs(2000);
      sh(args.serial, 'logcat -c', { timeoutMs: 20000 });
      ctx.snap('launch on Metro', sh(args.serial, `am start -n ${ACTIVITY}`, { timeoutMs: 30000 }));
      sleepMs(55000);
      let log = logcatDump(args.serial, ctx, 'logcat after launch');
      ctx.note('JS bundle executed (app-mounted)', /app-mounted/.test(log), grepLines(log, /app-mounted/, 1).join(' ').slice(0, 160));
      ctx.note('no FATAL EXCEPTION for consumer', !new RegExp(`Process: ${PKG}`).test(log), 'host-matched over full logcat');
      ctx.note('no UnsatisfiedLinkError', !/UnsatisfiedLinkError/.test(log), 'host-matched over full logcat');
      // First launch after install may show the runtime permission dialog.
      // The grant state is asserted from the device-observed logcat below
      // ([UBM_PROBE] permission-ok=true), never from the tap outcome: a
      // missing dialog means already-granted, which the log confirms.
      // Tap by exact text: the PROBE_TAP fixed coords predate this AVD
      // boot's density and miss every button (R01FACTORY proved text taps).
      const inited = tapText(args.serial, ctx, 'T8', 'INIT MANAGER');
      ctx.note('Init button tapped', inited !== null, inited ? `tapped at ${inited}` : 'INIT MANAGER missing from UI dump');
      sleepMs(6000);
      const allowed = tapText(args.serial, ctx, 'T8', 'Allow');
      sleepMs(16000);
      log = logcatDump(args.serial, ctx, 'logcat after init');
      const permLine = grepLines(log, /permission-ok=|scan=granted/, 2).join(' ');
      ctx.note('runtime permissions granted (dialog answered or already granted)', /permission-ok=true/.test(log), `${allowed ? `Allow tapped at ${allowed}; ` : 'no dialog (already granted); '}${permLine || 'no permission line'}`);
      ctx.note('manager created through the production binding (post-flip native route)', /manager-created/.test(log), grepLines(log, /manager-created|init-error/, 1).join(' ').slice(0, 200));
      ctx.note('adapter state round-trips through the production binding', /adapter power=/.test(log), grepLines(log, /adapter power=/, 1).join(' ').slice(0, 200));
      const bondedTap = tapText(args.serial, ctx, 'T8', 'BONDED PEERS');
      ctx.note('Bonded button tapped', bondedTap !== null, bondedTap ? `tapped at ${bondedTap}` : 'BONDED PEERS missing from UI dump');
      sleepMs(14000);
      log = logcatDump(args.serial, ctx, 'logcat after bonded');
      ctx.note('bonded reports the empty table without errors (no core bond surface yet; R03)', /bonded-count=0/.test(log), grepLines(log, /bonded-(count|error)/, 1).join(' ').slice(0, 220));
      const abortedTap = tapText(args.serial, ctx, 'T8', 'ABORTED FIND');
      ctx.note('Aborted-find button tapped', abortedTap !== null, abortedTap ? `tapped at ${abortedTap}` : 'ABORTED FIND missing from UI dump');
      sleepMs(14000);
      log = logcatDump(args.serial, ctx, 'logcat after aborted find');
      ctx.note('pre-aborted find settles as operation.aborted on-device', /aborted-find-settled[^]*operation\.aborted/.test(log), grepLines(log, /aborted-find-settled/, 1).join(' ').slice(0, 220));
      const scanTap = tapText(args.serial, ctx, 'T8', 'SCAN 8S THEN CANCEL');
      ctx.note('Scan button tapped', scanTap !== null, scanTap ? `tapped at ${scanTap}` : 'SCAN 8S THEN CANCEL missing from UI dump');
      sleepMs(14000);
      log = logcatDump(args.serial, ctx, 'logcat after scan');
      const scanLine = grepLines(log, /\[UBM_PROBE\] find-settled/, 1).join(' ');
      ctx.note('scan settles with an explicit typed error (no hang, nothing swallowed)', /\[UBM_PROBE\] find-settled name=\S+/.test(scanLine), scanLine.slice(0, 220));
      ctx.boundary('Mid-scan AbortSignal cancellation needs a pending scan; scans reject immediately on the virtual adapter, so no native queue forms. Pending-scan cancellation stays a U-ANDROID-PHYSICAL follow-up.');
    });
  }

  if (want('RUSTCORE')) {
    check(records, 'RUSTCORE', 'producer binding session: openSession/invoke/close through the real TurboModule + JNI core', 'REAL-EMULATOR', (ctx) => {
      if (!args.attemptMetro) {
        ctx.boundary('Needs the probe Metro (`npx react-native start --port 8081` in emulator-probe/consumer + `adb reverse tcp:8081 tcp:8081`); rerun with --attempt-metro. Dev APK has no embedded bundle.');
        return;
      }
      ctx.note('metro packager reachable [HOST-JVM host-side]', metroUpSync(), 'host-side curl http://127.0.0.1:8081/status (not device-observed)');
      sh(args.serial, `am force-stop ${PKG}`, { timeoutMs: 30000 });
      sleepMs(2000);
      sh(args.serial, 'logcat -c', { timeoutMs: 20000 });
      ctx.snap('launch on Metro', sh(args.serial, `am start -n ${ACTIVITY}`, { timeoutMs: 30000 }));
      let log = '';
      let mounted = false;
      for (let i = 0; i < 9; i++) {
        sleepMs(10000);
        log = logcatDump(args.serial, ctx, `logcat mount poll ${i}`);
        if (/app-mounted/.test(log)) { mounted = true; break; }
      }
      ctx.note('JS bundle executed (app-mounted)', mounted, grepLines(log, /app-mounted|rustcore-error/, 2).join(' ').slice(0, 200) || 'no mount line');
      if (!mounted) return;
      // The RustCore button is the 5th stacked button (no fixed PROBE_TAP
      // coords; T8's first-four coords are unaffected): tap by exact text.
      // Android renders RN Button titles uppercased in the UI dump.
      const tapped = tapText(args.serial, ctx, 'RUSTCORE', 'RUSTCORE SESSION');
      ctx.note('RustCore session button tapped', tapped !== null, tapped ? `tapped at ${tapped}` : 'button text missing from UI dump');
      if (!tapped) return;
      sleepMs(14000);
      log = logcatDump(args.serial, ctx, 'logcat after rustcore session');
      const okLine = grepLines(log, /rustcore-ok/, 1).join(' ');
      const errLine = grepLines(log, /rustcore-error/, 1).join(' ');
      // grepLines truncates to 300 chars; the receipt line carries late
      // tokens (scanStop, closed) past that cut, so match them on the full
      // untruncated line. Both tokens are emitted only by this receipt.
      const fullOk = (log.match(/\[UBM_PROBE\] rustcore-ok[^\n]*/) || [''])[0];
      ctx.note('session opened on the frozen contract revision', /rustcore-ok contract=C-UBM\.0\.1\.2-DRAFT/.test(log), okLine.slice(0, 240) || errLine.slice(0, 240) || 'neither rustcore-ok nor rustcore-error');
      ctx.note('central.status round-trips core-minted JSON', /live_operations/.test(okLine), okLine.slice(0, 240));
      ctx.note('echo.counter round-trips decimal 41', /echo=41/.test(okLine), okLine.slice(0, 240));
      ctx.note('scan.start mints a core op id', /scanOp=\S+/.test(okLine), okLine.slice(0, 240));
      ctx.note('scan.take drains staged scan steps', /scan\.start/.test(okLine), okLine.slice(0, 240));
      ctx.note('scan.stop settles the minted op', /"event":"stop"/.test(fullOk), fullOk.slice(-160));
      ctx.note('session closed cleanly', /closed=true/.test(fullOk), fullOk.slice(-160));
      ctx.note('no UnsatisfiedLinkError', !/UnsatisfiedLinkError/.test(log), 'host-matched over full logcat');
    });
  }

  if (want('R01FACTORY')) {
    check(records, 'R01FACTORY', 'R01 flip: public no-options factory creates + adapter + destroy through the production binding', 'REAL-EMULATOR', (ctx) => {
      if (!args.attemptMetro) {
        ctx.boundary('Needs the probe Metro (`npx react-native start --port 8081` in emulator-probe/consumer + `adb reverse tcp:8081 tcp:8081`); rerun with --attempt-metro. Dev APK has no embedded bundle.');
        return;
      }
      ctx.note('metro packager reachable [HOST-JVM host-side]', metroUpSync(), 'host-side curl http://127.0.0.1:8081/status (not device-observed)');
      sh(args.serial, `am force-stop ${PKG}`, { timeoutMs: 30000 });
      sleepMs(2000);
      sh(args.serial, 'logcat -c', { timeoutMs: 20000 });
      ctx.snap('launch on Metro', sh(args.serial, `am start -n ${ACTIVITY}`, { timeoutMs: 30000 }));
      let log = '';
      let mounted = false;
      for (let i = 0; i < 9; i++) {
        sleepMs(10000);
        log = logcatDump(args.serial, ctx, `logcat mount poll ${i}`);
        if (/app-mounted/.test(log)) { mounted = true; break; }
      }
      ctx.note('JS bundle executed (app-mounted)', mounted, grepLines(log, /app-mounted|init-error/, 2).join(' ').slice(0, 200) || 'no mount line');
      if (!mounted) return;
      // Tap by exact text (like RUSTCORE): PROBE_TAP coords predate this
      // AVD boot's density and miss every button.
      const inited = tapText(args.serial, ctx, 'R01FACTORY', 'INIT MANAGER');
      ctx.note('Init button tapped', inited !== null, inited ? `tapped at ${inited}` : 'button text missing from UI dump');
      if (!inited) return;
      sleepMs(6000);
      tapText(args.serial, ctx, 'R01FACTORY', 'Allow');
      sleepMs(16000);
      log = logcatDump(args.serial, ctx, 'logcat after init');
      const created = /manager-created/.test(log);
      ctx.note('manager created through the public no-options factory', created, grepLines(log, /manager-created|init-error/, 1).join(' ').slice(0, 220) || 'neither manager-created nor init-error');
      ctx.note('runtime permissions granted (dialog answered)', /permission-ok=true/.test(log), grepLines(log, /permission-ok=|scan=granted/, 2).join(' ').slice(0, 220) || 'no permission line');
      // Native-reader marker: with permissions granted the router's platform
      // reader reports a concrete state (never unknown); the retired 4.x
      // bridge historically reports unknowns on the virtual adapter.
      const adapterLine = grepLines(log, /adapter power=/, 1).join(' ');
      ctx.note(
        'adapter state is a concrete platform read (native reader)',
        /adapter power=(on|off|resetting|unsupported) availability=\S+ authorization=granted/.test(adapterLine),
        adapterLine.slice(0, 220) || 'no adapter line'
      );
      ctx.note('no init-error', !/init-error/.test(log), grepLines(log, /init-error/, 1).join(' ').slice(0, 220) || 'clean');
      if (!created) return;
      const bondedTap = tapText(args.serial, ctx, 'R01FACTORY', 'BONDED PEERS');
      ctx.note('Bonded button tapped', bondedTap !== null, bondedTap ? `tapped at ${bondedTap}` : 'button text missing from UI dump');
      if (!bondedTap) return;
      sleepMs(8000);
      log = logcatDump(args.serial, ctx, 'logcat after bonded');
      // Informational, not a route marker: the native provider reports the
      // empty bond table locally (no core bond surface yet; R03), which
      // coincides with the legacy empty-table receipt.
      ctx.note(
        'bonded reports the empty table without errors',
        /bonded-count=0/.test(log) && !/bonded-error/.test(log),
        grepLines(log, /bonded-(error|count)/, 1).join(' ').slice(0, 220) || 'no bonded line'
      );
      const torn = tapText(args.serial, ctx, 'R01FACTORY', 'TEARDOWN');
      ctx.note('Teardown button tapped', torn !== null, torn ? `tapped at ${torn}` : 'button text missing from UI dump');
      if (!torn) return;
      sleepMs(8000);
      log = logcatDump(args.serial, ctx, 'logcat after teardown');
      ctx.note('manager destroyed through the production binding', /manager-destroyed/.test(log), grepLines(log, /manager-destroyed|teardown-error/, 1).join(' ').slice(0, 220) || 'no teardown line');
      ctx.note('no teardown-error', !/teardown-error/.test(log), grepLines(log, /teardown-error/, 1).join(' ').slice(0, 220) || 'clean');
      ctx.note('no FATAL EXCEPTION for consumer', !new RegExp(`Process: ${PKG}`).test(log), 'host-matched over full logcat');
      ctx.note('no UnsatisfiedLinkError', !/UnsatisfiedLinkError/.test(log), 'host-matched over full logcat');
    });
  }

  if (want('T11')) {
    check(records, 'T11', 'bounded byte delivery (512 records / 1 MiB ingress bound)', 'REAL-EMULATOR', (ctx) => {
      ctx.boundary('No notification burst can be driven on the virtual adapter (no peers, scans reject fast), so the bounded ingress queue cannot fill on-emulator. Overflow semantics are covered host-side by the deterministic TCK vector deterministic-tck-subscription-overflow.ts plus the 104-test android unit suite; filling the queue needs a chatty physical peripheral (U-ANDROID-PHYSICAL).');
    });
  }

  if (want('T12')) {
    check(records, 'T12', 'JS reload: dev-menu Reload re-mounts and rebinds the native module in-process', 'REAL-EMULATOR', (ctx) => {
      if (!args.attemptMetro) {
        ctx.boundary('Needs the probe Metro; rerun with --attempt-metro.');
        return;
      }
      sh(args.serial, 'logcat -c', { timeoutMs: 20000 });
      const pidBefore = pidof(args.serial, ctx, 'pidof before reload');
      ctx.note('live process before reload', /^\d+$/.test(pidBefore), `pid=${pidBefore}`);
      ctx.snap('open dev menu (KEYCODE_MENU)', sh(args.serial, 'input keyevent 82', { timeoutMs: 30000 }));
      sleepMs(4000);
      const reloaded = tapText(args.serial, ctx, 'T12', 'Reload');
      ctx.note('dev-menu Reload tapped', reloaded !== null, reloaded ? `Reload at ${reloaded}` : 'menu item missing');
      // The fresh bundle rebuilds on Metro; poll for the remount. logcat was
      // cleared at the start, so exactly one post-clear mount IS the remount.
      let log = '';
      let mounts = 0;
      for (let i = 0; i < 10; i++) {
        sleepMs(10000);
        log = logcatDump(args.serial, ctx, `logcat reload poll ${i}`);
        mounts = (log.match(/app-mounted/g) || []).length;
        if (mounts >= 1) break;
      }
      ctx.note('app-mounted after Reload (fresh JS + native rebind)', mounts >= 1, `app-mounted x${mounts}`);
      const pidAfter = pidof(args.serial, ctx, 'pidof after reload');
      ctx.note('same process across reload (in-process rebind, no relaunch)', pidAfter === pidBefore && /^\d+$/.test(pidAfter), `${pidBefore} -> ${pidAfter}`);
      ctx.note('no FATAL EXCEPTION across reload', !new RegExp(`Process: ${PKG}`).test(log), 'host-matched over full logcat');
    });
  }

  const summary = lib.summarizeBattery(records);
  fs.mkdirSync(RESULTS_DIR, { recursive: true });
  const out = {
    suite: 'ubm5-android-emulator-battery-v1',
    avd: 'ubm5-emu-probe',
    serial: args.serial,
    package: PKG,
    consumer: `${PKG} debug APK built from emulator-probe/consumer (real 4.x native module path; Expo Go and JS mocks excluded)`,
    nativeModule: 'UnifiedBleProtocolControl via BlePlxPackage; libunified_ble_native_protocol.so (x86_64)',
    rustSwapInPoint: 'emulator-probe/README.md documents substituting the 5.0 Rust artifact for android/ JNI libs',
    startedAt: new Date().toISOString(),
    records,
    summary,
  };
  const outPath = args.outFile
    ? (path.isAbsolute(args.outFile) ? args.outFile : path.join(REPO_ROOT, args.outFile))
    : path.join(RESULTS_DIR, 'battery.json');
  fs.mkdirSync(path.dirname(outPath), { recursive: true });
  fs.writeFileSync(outPath, JSON.stringify(out, null, 2) + '\n');
  console.log(JSON.stringify(summary));
  const failed = records.filter((r) => r.result === 'fail');
  if (failed.length > 0) {
    console.log(`FAIL: ${failed.map((r) => r.id).join(',')}`);
    process.exitCode = 1;
  }
}

function metroUpSync() {
  const res = spawnSync('curl', ['-s', '-m', '8', 'http://127.0.0.1:8081/status'], { encoding: 'utf8', timeout: 15000 });
  return /packager-status:running/.test(res.stdout || '');
}

main().catch((err) => {
  console.error(`battery harness error: ${(err && err.message) || err}`);
  process.exitCode = 2;
});
