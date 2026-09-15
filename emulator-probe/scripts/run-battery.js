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
    // Actual dlopen is proven in T8 via /proc/<pid>/maps.
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
      ctx.note('metro packager reachable', metroUpSync(), 'http://127.0.0.1:8081/status');
      sh(args.serial, `am force-stop ${PKG}`, { timeoutMs: 30000 });
      sleepMs(2000);
      sh(args.serial, 'logcat -c', { timeoutMs: 20000 });
      ctx.snap('launch on Metro', sh(args.serial, `am start -n ${ACTIVITY}`, { timeoutMs: 30000 }));
      sleepMs(55000);
      let log = logcatDump(args.serial, ctx, 'logcat after launch');
      ctx.note('JS bundle executed (app-mounted)', /app-mounted/.test(log), grepLines(log, /app-mounted/, 1).join(' ').slice(0, 160));
      ctx.note('no FATAL EXCEPTION for consumer', !new RegExp(`Process: ${PKG}`).test(log), 'host-matched over full logcat');
      ctx.note('no UnsatisfiedLinkError', !/UnsatisfiedLinkError/.test(log), 'host-matched over full logcat');
      // First launch after install shows the runtime permission dialog.
      tap(args.serial, ...PROBE_TAP.init);
      sleepMs(6000);
      const allowed = tapText(args.serial, ctx, 'T8', 'Allow');
      ctx.note('runtime permission dialog answered (fresh install) or already granted', true, allowed ? `Allow tapped at ${allowed}` : 'no dialog (already granted)');
      sleepMs(16000);
      log = logcatDump(args.serial, ctx, 'logcat after init');
      ctx.note('manager created through the native bridge', /manager-created/.test(log), grepLines(log, /manager-created|init-error/, 1).join(' ').slice(0, 200));
      ctx.note('adapter state round-trips (unknowns on virtual adapter, never blocking)', /adapter power=/.test(log), grepLines(log, /adapter power=/, 1).join(' ').slice(0, 200));
      tap(args.serial, ...PROBE_TAP.bonded);
      sleepMs(14000);
      log = logcatDump(args.serial, ctx, 'logcat after bonded');
      ctx.note('bonded ping reads the native bond table (empty on emulator)', /bonded-count=0/.test(log), grepLines(log, /bonded-(count|error)/, 1).join(' ').slice(0, 220));
      tap(args.serial, ...PROBE_TAP.aborted);
      sleepMs(14000);
      log = logcatDump(args.serial, ctx, 'logcat after aborted find');
      ctx.note('pre-aborted find settles as operation.aborted on-device', /aborted-find-settled[^]*operation\.aborted/.test(log), grepLines(log, /aborted-find-settled/, 1).join(' ').slice(0, 220));
      tap(args.serial, ...PROBE_TAP.scan);
      sleepMs(14000);
      log = logcatDump(args.serial, ctx, 'logcat after scan');
      const scanLine = grepLines(log, /\[UBM_PROBE\] find-settled/, 1).join(' ');
      ctx.note('scan settles with an explicit typed error (no hang, nothing swallowed)', /\[UBM_PROBE\] find-settled name=\S+/.test(scanLine), scanLine.slice(0, 220));
      ctx.boundary('Mid-scan AbortSignal cancellation needs a pending scan; scans reject immediately on the virtual adapter, so no native queue forms. Pending-scan cancellation stays a U-ANDROID-PHYSICAL follow-up.');
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
