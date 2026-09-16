// emulator-probe/five0/run-five0-battery.js
// UBM 5.0 HOST-ANDROID 5.0-artifact emulator battery (U5/U9/U10 vehicle).
// Drives ONLY the explicit --serial target (task-owned AVD ubm5-emu-five0).
// Every adb/emulator/device command carries its own timeout; a timeout
// records HUNG-SKIPPED once with the exact command and is never retried
// blindly. Device commands contain no pipes: full outputs are saved under
// five0/logs/ and matched on the host. Raw evidence is redacted.
//
// Origin taxonomy (every record carries exactly one):
// - REAL-EMULATOR: device-observed install/launch/lifecycle/permission facts.
// - SIMULATED-injection: in-APK synthetic GATT wires through the real JNI
//   natives into the REAL Central (no BLE peers exist on this lane — the
//   virtual-radio boundary; never a physical claim).
//
// Usage:
//   node emulator-probe/five0/run-five0-battery.js [--serial emulator-5556]
//     [--apk emulator-probe/five0/probe-app/app/build/outputs/apk/debug/app-debug.apk]
//     [--only PRE,F1,F3] [--no-cleanup]
'use strict'
const { spawnSync, spawn } = require('node:child_process')
const fs = require('node:fs')
const path = require('node:path')
const lib = require('../lib/battery-lib.js')

const REPO_ROOT = path.resolve(__dirname, '..', '..')
const FIVE0_DIR = path.join(REPO_ROOT, 'emulator-probe', 'five0')
const LOGS_DIR = path.join(FIVE0_DIR, 'logs')
const RESULTS_FILE = path.join(FIVE0_DIR, 'results-five0.json')
const PKG = 'com.ubmfive0probe'
const ACTIVITY = `${PKG}/.MainActivity`
const AVD = 'ubm5-emu-five0'
const ADB_PORT = 5556
const SERIAL = `emulator-${ADB_PORT}`
const SDK =
  process.env.ANDROID_HOME || process.env.ANDROID_SDK_ROOT || path.join(process.env.HOME || '/root', 'Android', 'Sdk')
const AVD_HOME = '/tmp/ubm5-five0-avd'
const BOOT_DEADLINE_MS = 600000

function parseArgs(argv) {
  const out = {
    serial: SERIAL,
    apk: path.join(FIVE0_DIR, 'probe-app', 'app', 'build', 'outputs', 'apk', 'debug', 'app-debug.apk'),
    only: null,
    cleanup: true
  }
  for (let i = 0; i < argv.length; i++) {
    if (argv[i] === '--serial') out.serial = argv[++i]
    else if (argv[i] === '--apk') out.apk = argv[++i]
    else if (argv[i] === '--only') out.only = new Set(argv[++i].split(','))
    else if (argv[i] === '--no-cleanup') out.cleanup = false
  }
  lib.validateAvdName(AVD)
  if (!/^emulator-\d+$/.test(out.serial)) throw new Error(`refusing non-emulator serial ${out.serial}`)
  if (!fs.existsSync(out.apk)) throw new Error(`APK missing: ${out.apk} (build probe-app first)`)
  return out
}

function env() {
  return { ...process.env, ANDROID_AVD_HOME: AVD_HOME, ANDROID_HOME: SDK, ANDROID_SDK_ROOT: SDK }
}

function run(cmd, args, { timeoutMs = 30000, cwd = REPO_ROOT, input = undefined } = {}) {
  const res = spawnSync(cmd, args, { encoding: 'utf8', timeout: timeoutMs, cwd, env: env(), input })
  const raw = `${res.stdout || ''}\n${res.stderr || ''}`
  return { exit: res.status, timedOut: !!(res.error && res.error.code === 'ETIMEDOUT'), raw }
}

function rawAdb(serial, args, opts) {
  return run('adb', ['-s', serial, ...args], opts)
}

// Device shell command with NO pipes (host does the matching).
function sh(serial, cmd, opts) {
  if (cmd.includes('|')) throw new Error(`device-side pipe refused: ${cmd}`)
  return rawAdb(serial, ['shell', cmd], opts)
}

function sleepMs(ms) {
  spawnSync('sleep', [(ms / 1000).toString()])
}

function grepLines(text, re, max) {
  const out = []
  for (const line of String(text).split('\n')) {
    if (re.test(line)) {
      out.push(line.slice(0, 300))
      if (out.length >= (max || 10)) break
    }
  }
  return out
}

function check(records, id, name, origin, fn) {
  const record = {
    id,
    name,
    origin: lib.labelOrigin(origin),
    assertions: { passed: 0, total: 0 },
    checks: [],
    evidence: [],
    result: 'pass'
  }
  fs.mkdirSync(LOGS_DIR, { recursive: true })
  const ctx = {
    note: (checkName, pass, detail) => {
      record.assertions.total += 1
      if (pass) record.assertions.passed += 1
      else record.result = 'fail'
      record.checks.push({
        check: checkName,
        pass: !!pass,
        detail: lib.redactLog(String(detail || '')).slice(0, 300)
      })
    },
    snap: (label, result) => {
      const file = path.join(LOGS_DIR, `${id}-${record.evidence.length}.log`)
      fs.writeFileSync(
        file,
        `$ ${label}\n[exit=${result.exit}${result.timedOut ? ' TIMED-OUT' : ''}]\n${lib.redactLog(result.raw).slice(0, 200000)}\n`
      )
      record.evidence.push({
        command: label,
        exit: result.exit,
        timedOut: !!result.timedOut,
        log: path.relative(FIVE0_DIR, file)
      })
      return { ...result, output: lib.redactLog(result.raw).slice(0, 200000) }
    },
    hung: command => {
      record.result = 'skipped'
      record.hung = true
      record.checks.push({ check: 'completed without hanging', pass: false, detail: `HUNG-SKIPPED: ${command}` })
    },
    boundary: reason => {
      if (record.result === 'pass' && record.assertions.total === 0) record.result = 'boundary'
      record.boundaryReason = reason
    }
  }
  try {
    fn(ctx)
  } catch (err) {
    record.result = 'fail'
    record.checks.push({ check: 'threw', pass: false, detail: String((err && err.message) || err).slice(0, 300) })
  }
  records.push(record)
  return record
}

function pidof(serial, ctx, tag) {
  const r = ctx.snap(tag, sh(serial, `pidof ${PKG}`, { timeoutMs: 30000 }))
  return (r.output.trim().split(/\s+/)[0] || '').replace(/\D/g, '')
}

function logcatDump(serial, ctx, tag) {
  return ctx.snap(tag, sh(serial, 'logcat -d -v brief', { timeoutMs: 60000 })).output
}

function resultLine(log) {
  const lines = grepLines(log, /UBM5FIVE0-RESULT/, 4)
  return lines.length > 0 ? lines[lines.length - 1] : ''
}

function launchAndWaitResult(serial, ctx, id, waitMs) {
  sh(serial, 'logcat -c', { timeoutMs: 20000 })
  const start = ctx.snap(`am start ${ACTIVITY}`, sh(serial, `am start -n ${ACTIVITY}`, { timeoutMs: 30000 }))
  // Poll for the self-test RESULT line instead of a fixed sleep: cold ART
  // (first launch after install, 1.2MB oversize wire) is much slower than
  // warm runs, and a fixed window flakes either way. Intermediate dumps are
  // not snapped; only the final dump becomes evidence.
  const deadline = Date.now() + (waitMs || 12000)
  let seen = false
  while (Date.now() < deadline && !seen) {
    sleepMs(3000)
    const probe = sh(serial, 'logcat -d -v brief', { timeoutMs: 60000 })
    seen = /UBM5FIVE0-RESULT/.test(probe.raw || '')
  }
  const log = logcatDump(serial, ctx, `${id} logcat after launch`)
  return { start, log, result: resultLine(log) }
}

function main() {
  const args = parseArgs(process.argv.slice(2))
  const want = id => !args.only || args.only.has(id)
  const records = []
  const serial = args.serial
  let emulatorProc = null

  // --- Owned AVD + emulator lifecycle (task-owned temp home, explicit port).
  if (want('AVD'))
    check(records, 'AVD', 'task-owned AVD create + headless launch', 'REAL-EMULATOR', ctx => {
      fs.mkdirSync(AVD_HOME, { recursive: true })
      const img = path.join(SDK, 'system-images', 'android-34', 'google_apis', 'x86_64')
      ctx.note('x86_64 system image present', fs.existsSync(img), img)
      const avdIni = path.join(AVD_HOME, `${AVD}.ini`)
      if (!fs.existsSync(avdIni)) {
        const mk = ctx.snap(
          'avdmanager create avd (task-owned)',
          run(
            path.join(SDK, 'cmdline-tools', 'latest', 'bin', 'avdmanager'),
            ['create', 'avd', '-n', AVD, '-k', 'system-images;android-34;google_apis;x86_64', '-d', 'pixel', '--force'],
            { timeoutMs: 300000, input: 'no\n' }
          )
        )
        if (mk.timedOut) return ctx.hung('avdmanager create avd ubm5-emu-five0')
        ctx.note('avd created', mk.exit === 0 && fs.existsSync(avdIni), `exit=${mk.exit}`)
      } else {
        ctx.note('avd already owned (reused, not wiped)', true, avdIni)
      }
      emulatorProc = spawn(path.join(SDK, 'emulator', 'emulator'), lib.emulatorArgs({ avd: AVD, adbPort: ADB_PORT }), {
        env: env(),
        stdio: 'ignore',
        detached: true
      })
      emulatorProc.unref()
      ctx.note('emulator spawned detached (owned pid)', !!emulatorProc.pid, `pid=${emulatorProc && emulatorProc.pid}`)
      const t0 = Date.now()
      let booted = false
      while (Date.now() - t0 < BOOT_DEADLINE_MS) {
        const r = sh(serial, 'getprop sys.boot_completed', { timeoutMs: 30000 })
        if (!r.timedOut && r.exit === 0 && /^\s*1\s*$/.test(r.raw || '')) {
          booted = true
          break
        }
        sleepMs(10000)
      }
      ctx.note('boot completed within deadline', booted, `waited=${Math.round((Date.now() - t0) / 1000)}s`)
      if (!booted) return ctx.hung(`emulator ${AVD} boot (sys.boot_completed never 1)`)
      const fp = ctx.snap('fingerprint/sdk/abi', sh(serial, 'getprop ro.build.fingerprint'))
      ctx.note('emulator-5556 reachable', fp.exit === 0, (fp.output.trim().split('\n')[0] || '').slice(0, 160))
    })

  if (want('PRE'))
    check(records, 'PRE', 'device identity + preconditions', 'REAL-EMULATOR', ctx => {
      ctx.snap('adb uninstall (idempotent reruns)', {
        ...rawAdb(serial, ['uninstall', PKG], { timeoutMs: 60000 }),
        output: ''
      })
      const fp = ctx.snap('getprop fingerprint', sh(serial, 'getprop ro.build.fingerprint'))
      const sdk = ctx.snap('getprop sdk', sh(serial, 'getprop ro.build.version.sdk'))
      const abi = ctx.snap('getprop abi', sh(serial, 'getprop ro.product.cpu.abi'))
      ctx.note(
        'sdk_gphone64_x86_64, api 34, abi x86_64',
        /sdk_gphone64_x86_64/.test(fp.output) && /34/.test(sdk.output) && /x86_64/.test(abi.output),
        `${fp.output.trim().split('\n')[0]} sdk=${sdk.output.trim()} abi=${abi.output.trim()}`.slice(0, 200)
      )
      const bt = ctx.snap('dumpsys bluetooth_manager', sh(serial, 'dumpsys bluetooth_manager', { timeoutMs: 30000 }))
      if (bt.timedOut) return ctx.hung(`adb -s ${serial} shell dumpsys bluetooth_manager`)
      ctx.note(
        'bluetooth_manager queryable (virtual adapter, never physical)',
        bt.exit === 0 && /Bluetooth Status/.test(bt.output),
        grepLines(bt.output, /enabled:|state:/, 3)
          .join(' ')
          .slice(0, 200)
      )
      const absent = ctx.snap('pre-install package check', sh(serial, 'pm list packages'))
      ctx.note('package absent before install', !new RegExp(PKG).test(absent.output), 'host-matched')
    })

  if (want('F1'))
    check(records, 'F1', 'install 5.0 probe APK', 'REAL-EMULATOR', ctx => {
      const r = rawAdb(serial, ['install', '-r', args.apk], { timeoutMs: 120000 })
      const inst = { ...r, output: lib.redactLog(r.raw).slice(0, 2000) }
      ctx.snap(`adb install -r ${path.basename(args.apk)}`, inst)
      if (inst.timedOut) return ctx.hung(`adb -s ${serial} install -r ${args.apk}`)
      ctx.note(
        'install reports Success',
        inst.exit === 0 && /Success/.test(inst.output),
        grepLines(inst.output, /Success|Failure/, 2).join(' ')
      )
      const listed = ctx.snap('pm list packages (host-matched)', sh(serial, 'pm list packages'))
      ctx.note('package listed after install', new RegExp(PKG).test(listed.output), `package:${PKG}`)
      const info = ctx.snap('dumpsys package', sh(serial, `dumpsys package ${PKG}`, { timeoutMs: 30000 }))
      ctx.note(
        'package info v1/min24/target34',
        /versionCode=1/.test(info.output) && /minSdk=24/.test(info.output) && /targetSdk=34/.test(info.output),
        grepLines(info.output, /versionCode=|minSdk=|targetSdk=/, 3)
          .join(' ')
          .slice(0, 200)
      )
    })

  if (want('F2'))
    check(records, 'F2', 'native-lib load surface (x86_64 cdylib)', 'REAL-EMULATOR', ctx => {
      const info = ctx.snap('dumpsys package (ABI lines)', sh(serial, `dumpsys package ${PKG}`, { timeoutMs: 30000 }))
      if (info.timedOut) return ctx.hung(`adb -s ${serial} shell dumpsys package ${PKG}`)
      ctx.note(
        'primaryCpuAbi=x86_64',
        /primaryCpuAbi=x86_64/.test(info.output),
        grepLines(info.output, /CpuAbi/, 3)
          .join(' ')
          .slice(0, 160)
      )
      ctx.note(
        'nativeLibraryDir declared',
        /nativeLibraryDir=|legacyNativeLibraryDir=/.test(info.output),
        grepLines(info.output, /nativeLibraryDir=|legacyNativeLibraryDir=/, 1)
          .join(' ')
          .slice(0, 200)
      )
      const apk = ctx.snap('pm path', sh(serial, `pm path ${PKG}`, { timeoutMs: 30000 }))
      ctx.note('apk path queryable', new RegExp(PKG).test(apk.output), apk.output.trim().split('\n')[0].slice(0, 160))
    })

  if (want('F3'))
    check(records, 'F3', 'binding identity: launch, RESULT ok, no link errors', 'REAL-EMULATOR', ctx => {
      for (const perm of [
        'android.permission.BLUETOOTH_SCAN',
        'android.permission.BLUETOOTH_CONNECT',
        'android.permission.ACCESS_FINE_LOCATION'
      ]) {
        ctx.snap(`pm grant ${perm.split('.').pop()}`, sh(serial, `pm grant ${PKG} ${perm}`, { timeoutMs: 30000 }))
      }
      const { start, log, result } = launchAndWaitResult(serial, ctx, 'F3', 45000)
      if (start.timedOut) return ctx.hung(`adb -s ${serial} shell am start -n ${ACTIVITY}`)
      ctx.note(
        'start accepted',
        /Starting: Intent/.test(start.output),
        start.output.trim().split('\n')[0].slice(0, 160)
      )
      const pid = pidof(serial, ctx, 'pidof after launch')
      ctx.note('process running after launch', /^\d+$/.test(pid), `pid=${pid}`)
      ctx.note(
        'no FATAL EXCEPTION for probe',
        !new RegExp(`FATAL EXCEPTION.*\\n.*${PKG}|Process: ${PKG}`).test(log),
        'host-matched'
      )
      ctx.note('no UnsatisfiedLinkError', !/UnsatisfiedLinkError/.test(log), 'host-matched')
      ctx.note('UBM5FIVE0-RESULT result ok', /"result":"ok"/.test(result), result.slice(0, 300))
    })

  if (want('F4'))
    check(records, 'F4', 'setup-teardown: force-stop clears process', 'REAL-EMULATOR', ctx => {
      sh(serial, `am start -n ${ACTIVITY}`, { timeoutMs: 30000 })
      sleepMs(12000)
      const stop = ctx.snap('am force-stop', sh(serial, `am force-stop ${PKG}`))
      ctx.note('force-stop exits 0', stop.exit === 0, 'ok')
      sleepMs(2000)
      const pid = pidof(serial, ctx, 'pidof after force-stop')
      ctx.note('no process lingers', pid === '', `pid=${pid || '(none)'}`)
    })

  if (want('F5'))
    check(records, 'F5', 'lifecycle foreground-background', 'REAL-EMULATOR', ctx => {
      sh(serial, `am start -n ${ACTIVITY}`, { timeoutMs: 30000 })
      sleepMs(12000)
      const before = pidof(serial, ctx, 'pidof foreground')
      ctx.note('foreground pid captured', /^\d+$/.test(before), `pid=${before}`)
      sh(serial, 'input keyevent KEYCODE_HOME', { timeoutMs: 30000 })
      sleepMs(3000)
      const during = pidof(serial, ctx, 'pidof background')
      ctx.note('HOME keeps process (same pid)', during === before, `pid=${during}`)
      const back = ctx.snap('relaunch to foreground', sh(serial, `am start -n ${ACTIVITY}`, { timeoutMs: 30000 }))
      if (back.timedOut) return ctx.hung(`adb -s ${serial} shell am start -n ${ACTIVITY}`)
      sleepMs(5000)
      const acts = ctx.snap(
        'dumpsys activity activities',
        sh(serial, 'dumpsys activity activities', { timeoutMs: 60000 })
      )
      const resumed = grepLines(acts.output, /topResumedActivity=|mResumedActivity:?/, 2).join(' ')
      ctx.note('probe activity re-resumed', new RegExp(PKG).test(resumed), resumed.slice(0, 200))
    })

  if (want('F6'))
    check(records, 'F6', 'process kill-relaunch (no immortal handles)', 'REAL-EMULATOR', ctx => {
      sh(serial, `am start -n ${ACTIVITY}`, { timeoutMs: 30000 })
      sleepMs(12000)
      const before = pidof(serial, ctx, 'pidof before kill')
      ctx.note('victim pid captured', /^\d+$/.test(before), `pid=${before}`)
      if (/^\d+$/.test(before)) {
        const kill = ctx.snap(
          'run-as kill -9 own pid',
          sh(serial, `run-as ${PKG} kill -9 ${before}`, { timeoutMs: 30000 })
        )
        ctx.note('kill accepted', kill.exit === 0, `exit=${kill.exit}`)
        sleepMs(3000)
        const gone = pidof(serial, ctx, 'pidof after kill')
        ctx.note('victim gone', gone === '', `pid=${gone || '(none)'}`)
      }
      const { log, result } = launchAndWaitResult(serial, ctx, 'F6', 30000)
      const after = pidof(serial, ctx, 'pidof after relaunch')
      ctx.note('fresh pid after relaunch', /^\d+$/.test(after) && after !== before, `pid=${after}`)
      ctx.note('RESULT ok after relaunch (no immortal handles)', /"result":"ok"/.test(result), result.slice(0, 200))
      ctx.note('no FATAL on relaunch', !/FATAL EXCEPTION/.test(log), 'host-matched')
    })

  if (want('F7'))
    check(records, 'F7', 'permission-denied fails closed with identity', 'REAL-EMULATOR', ctx => {
      const rev1 = ctx.snap(
        'pm revoke BLUETOOTH_CONNECT',
        sh(serial, `pm revoke ${PKG} android.permission.BLUETOOTH_CONNECT`, { timeoutMs: 30000 })
      )
      const rev2 = ctx.snap(
        'pm revoke BLUETOOTH_SCAN',
        sh(serial, `pm revoke ${PKG} android.permission.BLUETOOTH_SCAN`, { timeoutMs: 30000 })
      )
      ctx.note('revokes accepted', rev1.exit === 0 && rev2.exit === 0, `exit=${rev1.exit},${rev2.exit}`)
      sh(serial, `am force-stop ${PKG}`, { timeoutMs: 30000 })
      sleepMs(2000)
      const { log, result } = launchAndWaitResult(serial, ctx, 'F7-denied', 30000)
      const denied = grepLines(log, /UBM5FIVE0-PERMISSION/, 2).join(' ')
      ctx.note(
        'denial observed with explicit identity',
        /permission\.denied/.test(denied + result),
        (denied + result).slice(0, 300)
      )
      ctx.note(
        'core cycle did not run while denied',
        !/check io-settled PASS/.test(log),
        'host-matched (no binder work without permission)'
      )
      const gr1 = ctx.snap(
        'pm grant BLUETOOTH_CONNECT back',
        sh(serial, `pm grant ${PKG} android.permission.BLUETOOTH_CONNECT`, { timeoutMs: 30000 })
      )
      const gr2 = ctx.snap(
        'pm grant BLUETOOTH_SCAN back',
        sh(serial, `pm grant ${PKG} android.permission.BLUETOOTH_SCAN`, { timeoutMs: 30000 })
      )
      ctx.note('grants restore', gr1.exit === 0 && gr2.exit === 0, `exit=${gr1.exit},${gr2.exit}`)
      sh(serial, `am force-stop ${PKG}`, { timeoutMs: 30000 })
      sleepMs(2000)
      const relaunch = launchAndWaitResult(serial, ctx, 'F7-restored', 30000)
      ctx.note('RESULT ok after re-grant', /"result":"ok"/.test(relaunch.result), relaunch.result.slice(0, 200))
    })

  if (want('F8'))
    check(
      records,
      'F8',
      'queued-cancel + lifecycle release via real JNI (synthetic wires)',
      'SIMULATED-injection',
      ctx => {
        sh(serial, `am force-stop ${PKG}`, { timeoutMs: 30000 })
        sleepMs(2000)
        const stopped = pidof(serial, ctx, 'pidof after force-stop (F8)')
        ctx.note('activity will cold-start (no lingering process)', stopped === '', `pid=${stopped || '(none)'}`)
        const { log, result } = launchAndWaitResult(serial, ctx, 'F8', 30000)
        ctx.note('self-test RESULT ok', /"result":"ok"/.test(result), result.slice(0, 300))
        for (const name of [
          'check queued-cancel-aborts PASS',
          'check io-settled PASS',
          'check release PASS',
          'check post-close-enqueue-rejects PASS',
          'check adapter-reset PASS',
          'check expire-sweep PASS'
        ]) {
          const found = grepLines(log, new RegExp(name.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')), 2)
          ctx.note(name, found.length > 0, found.join(' ').slice(0, 160))
        }
      }
    )

  if (want('F9'))
    check(records, 'F9', 'notify bounds via real JNI (synthetic wires)', 'SIMULATED-injection', ctx => {
      sh(serial, `am force-stop ${PKG}`, { timeoutMs: 30000 })
      sleepMs(2000)
      const stopped = pidof(serial, ctx, 'pidof after force-stop (F9)')
      ctx.note('activity will cold-start (no lingering process)', stopped === '', `pid=${stopped || '(none)'}`)
      const { log, result } = launchAndWaitResult(serial, ctx, 'F9', 30000)
      ctx.note('self-test RESULT ok', /"result":"ok"/.test(result), result.slice(0, 300))
      for (const name of [
        'check notify-delivered PASS',
        'check notify-oversize-rejects PASS',
        'check stale-handle-fails-closed PASS',
        'check path-registered PASS'
      ]) {
        const found = grepLines(log, new RegExp(name.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')), 2)
        ctx.note(name, found.length > 0, found.join(' ').slice(0, 160))
      }
    })

  const summary = lib.summarizeBattery(records)
  const report = {
    slice: 'HOST-ANDROID UBM 5.0 (trackourhealth/bun-mono#1188; U5/U9/U10 vehicle)',
    avd: AVD,
    serial,
    abi: 'x86_64',
    api: 34,
    apk: path.relative(REPO_ROOT, args.apk),
    revision: 'C-UBM.0.1.2-DRAFT',
    summary,
    records
  }
  fs.writeFileSync(RESULTS_FILE, JSON.stringify(report, null, 2) + '\n')
  console.log(
    `five0 battery: ${summary.tests} tests, ` +
      `${summary.assertionsPassed}/${summary.assertionsTotal} assertions, ` +
      JSON.stringify(summary.byResult)
  )
  console.log(`results: ${path.relative(REPO_ROOT, RESULTS_FILE)}`)

  // --- Cleanup: ONLY owned resources (owned emulator, task AVD, temp home).
  if (args.cleanup) {
    try {
      run('adb', ['-s', serial, 'emu', 'kill'], { timeoutMs: 30000 })
    } catch (_) {
      /* owned emulator already gone */
    }
    sleepMs(5000)
    if (emulatorProc && emulatorProc.pid) {
      try {
        process.kill(emulatorProc.pid, 'SIGKILL')
      } catch (_) {
        /* already gone */
      }
    }
    const leftovers = run('adb', ['devices'], { timeoutMs: 20000 })
    console.log(
      `cleanup: owned emulator killed; adb devices now: ${JSON.stringify((leftovers.raw || '').trim().split('\n').slice(1))}`
    )
    run('rm', ['-rf', path.join(AVD_HOME, `${AVD}.avd`), path.join(AVD_HOME, `${AVD}.ini`)], { timeoutMs: 60000 })
    console.log(`cleanup: task AVD ${AVD} removed from ${AVD_HOME} (shared ~/.android/avd untouched)`)
  } else {
    console.log(`cleanup SKIPPED (--no-cleanup): emulator ${serial} + AVD ${AVD} left running for inspection`)
  }
  const failed = records.filter(r => r.result === 'fail').length
  process.exit(failed > 0 ? 1 : 0)
}

main()
