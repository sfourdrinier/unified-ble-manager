// emulator-probe/u9/run-u9-battery.js — U9 restart-recovery E2E.
//
// Proves the durable restart contract on a real emulator: the foreground
// service persists the session-intent flag, process death keeps it, reboot
// auto-restarts the service iff the flag is set, and recovery fails closed
// (no flag → no restart). Task-owned AVD; committed record is
// results-u9.json (raw logs/ stays local-only).
//
// Precondition (run first, from the repo root):
//   cd example/android && ./gradlew :app:assembleDebug -PreactNativeArchitectures=x86_64
'use strict'
const { spawnSync, spawn } = require('node:child_process')
const fs = require('node:fs')
const path = require('node:path')
const lib = require('../lib/battery-lib')

const AVD = 'ubm5-emu-u9'
const SERIAL = 'emulator-5556'
const AVD_HOME = '/tmp/ubm5-u9-avd'
const SDK = process.env.ANDROID_HOME || path.join(process.env.HOME, 'Android', 'Sdk')
const PKG = 'com.bleplxexample'
const SERVICE = 'com.sfourdrinier.unifiedblemanager.BlePlxForegroundService'
const START_ACTION = 'com.sfourdrinier.unifiedblemanager.background.START'
const PREFS = 'shared_prefs/unified-ble-manager.xml'
const FLAG = 'unified-ble-manager.background.session-intent-exists'
const BOOT_DEADLINE_MS = 600000

const ROOT = path.resolve(__dirname, '..', '..')
const APK = path.join(ROOT, 'example/android/app/build/outputs/apk/debug/app-debug.apk')
const LOG_DIR = path.join(__dirname, 'logs')
const RESULTS = path.join(__dirname, 'results-u9.json')

function env() {
  return { ...process.env, ANDROID_AVD_HOME: AVD_HOME, ANDROID_HOME: SDK, ANDROID_SDK_ROOT: SDK }
}
function run(cmd, args, timeoutMs) {
  const r = spawnSync(cmd, args, { encoding: 'utf8', timeout: timeoutMs || 60000, env: env() })
  return {
    exit: r.status,
    timedOut: r.error && r.error.code === 'ETIMEDOUT',
    raw: `${r.stdout || ''}\n${r.stderr || ''}`
  }
}
function adb(args, t) {
  return run('adb', ['-s', SERIAL, ...args], t)
}
function sh(args, t) {
  return adb(['shell', ...args], t)
}
function sleepMs(ms) {
  spawnSync('sleep', [String(ms / 1000)])
}

function waitBoot(label) {
  const t0 = Date.now()
  for (;;) {
    const r = sh(['getprop sys.boot_completed'], 30000)
    if (!r.timedOut && r.exit === 0 && /^\s*1\s*$/.test(r.raw || '')) {
      return Math.round((Date.now() - t0) / 1000)
    }
    if (Date.now() - t0 > BOOT_DEADLINE_MS) throw new Error(`${label}: boot not detected in 600s`)
    sleepMs(10000)
  }
}

function serviceRunning() {
  const r = sh(['dumpsys activity services ' + PKG], 60000)
  return r.exit === 0 && new RegExp(SERVICE.replace(/\./g, '\\.')).test(r.raw || '')
}
function startService() {
  // Plain shell start: the DEBUG manifest (src/debug, U9 scaffolding only)
  // exports the service for cross-uid starts. Release stays exported=false;
  // production starts always come from inside the app uid.
  return sh([
    'am start-foreground-service -n ' +
      PKG +
      '/' +
      SERVICE +
      ' -a ' +
      START_ACTION +
      ' --es channelId ubm-u9 --es channelName "UBM U9" --es title "UBM" --es body "recovery" --ez restartSticky true'
  ])
}
function prefsFlag() {
  const r = sh(['run-as ' + PKG + ' cat ' + PREFS], 30000)
  if (r.exit !== 0) return 'absent'
  const m = (r.raw || '').match(new RegExp(`<boolean name="${FLAG}" value="(true|false)"`))
  return m ? m[1] : 'unparsed'
}
function fatals(since) {
  const log = adb(['shell', 'logcat -d -v brief'], 60000).raw || ''
  return { log, count: log.split('\n').filter(l => /FATAL EXCEPTION/.test(l) && l.includes(PKG)).length }
}
function relaunchApp() {
  // force-stop puts the app in stopped state (no BOOT_COMPLETED delivery).
  // Relaunching clears it — mirroring real use (user opens app, takes a
  // lease, later reboots). Required before every reboot assertion, positive
  // and negative alike, or the negative passes vacuously.
  sh(['am start -n ' + PKG + '/.MainActivity'], 30000)
  sleepMs(10000)
  return sh(['pidof ' + PKG], 30000).raw.trim() !== ''
}
function pollServiceRunning(seconds) {
  const t0 = Date.now()
  while (Date.now() - t0 < seconds * 1000) {
    if (serviceRunning()) return true
    sleepMs(5000)
  }
  return serviceRunning()
}

function main() {
  const only = (process.argv.find(a => a.startsWith('--only=')) || '').slice(7).split(',').filter(Boolean)
  const want = id => only.length === 0 || only.includes(id)
  const noCleanup = process.argv.includes('--no-cleanup')
  if (!fs.existsSync(APK)) throw new Error(`precondition APK missing: ${APK} (assembleDebug x86_64 first)`)
  fs.mkdirSync(LOG_DIR, { recursive: true })
  fs.mkdirSync(AVD_HOME, { recursive: true })

  const tLog = []
  const tag = s => {
    tLog.push(s)
    console.log(s)
  }
  const records = []
  function check(id, name, origin, fn) {
    const record = { id, name, origin, result: 'pass', assertions: { total: 0, passed: 0 }, checks: [] }
    const ctx = {
      snap: (n, r) => {
        const f = path.join(LOG_DIR, `${id}-${record.checks.length}.log`)
        fs.writeFileSync(f, `### ${n}\n### exit=${r.exit} timedOut=${!!r.timedOut}\n\n${lib.redactLog(r.raw || '')}`)
        return { exit: r.exit, output: r.raw || '' }
      },
      note: (checkName, pass, detail) => {
        record.assertions.total += 1
        if (pass) record.assertions.passed += 1
        else record.result = 'fail'
        record.checks.push({
          check: checkName,
          pass: !!pass,
          detail: lib.redactLog(String(detail || '')).slice(0, 300)
        })
      }
    }
    try {
      fn(ctx)
    } catch (e) {
      ctx.note('no harness exception', false, String((e && e.message) || e).slice(0, 200))
    }
    records.push(record)
  }

  // Owned AVD + boot.
  if (!fs.existsSync(path.join(AVD_HOME, `${AVD}.ini`))) {
    tag('creating owned AVD...')
    const mk = run(
      path.join(SDK, 'cmdline-tools', 'latest', 'bin', 'avdmanager'),
      ['create', 'avd', '-n', AVD, '-k', 'system-images;android-34;google_apis;x86_64', '-d', 'pixel', '--force'],
      300000
    )
    if (mk.exit !== 0) throw new Error('avdmanager failed:\n' + mk.raw)
  }
  const already = sh(['getprop sys.boot_completed'], 30000)
  if (!(/^\s*1\s*$/.test(already.raw || '') && already.exit === 0)) {
    tag('spawning owned emulator...')
    const p = spawn(
      path.join(SDK, 'emulator', 'emulator'),
      [
        '-avd',
        AVD,
        '-no-window',
        '-no-audio',
        '-no-boot-anim',
        '-gpu',
        'swiftshader_indirect',
        '-no-snapshot-save',
        '-port',
        '5556'
      ],
      { env: env(), stdio: 'ignore', detached: true }
    )
    p.unref()
  }
  const bootWait = waitBoot('initial')
  tag(`booted in ${bootWait}s`)

  if (want('PRE'))
    check('PRE', 'emulator identity', 'REAL-EMULATOR', ctx => {
      const fp = ctx.snap('fingerprint', sh(['getprop ro.build.fingerprint']))
      ctx.note('emulator reachable', fp.exit === 0, fp.output.trim().split('\n')[0].slice(0, 120))
      const inst = adb(['install', '-r', APK], 180000)
      ctx.note(
        'example APK installs',
        /Success/.test(inst.raw || ''),
        (inst.raw || '')
          .split('\n')
          .filter(l => /Success|Failure/.test(l))
          .join(' ')
      )
      adb(['shell', 'logcat -c'], 20000)
      const start = ctx.snap('launch MainActivity', sh(['am start -n ' + PKG + '/.MainActivity']))
      ctx.note(
        'activity launches (out of stopped state)',
        /Starting:/.test(start.output),
        start.output.trim().split('\n')[0].slice(0, 120)
      )
      sleepMs(15000)
    })

  if (want('U9-1'))
    check('U9-1', 'service start persists session-intent flag', 'REAL-EMULATOR', ctx => {
      // API 34 requires the runtime BLUETOOTH_CONNECT grant for an FGS of
      // type connectedDevice — same as a real user flow (grant, then start).
      sh(['pm grant ' + PKG + ' android.permission.BLUETOOTH_CONNECT'], 30000)
      sh(['pm grant ' + PKG + ' android.permission.BLUETOOTH_SCAN'], 30000)
      const start = ctx.snap('start foreground service (restartSticky=true)', startService())
      ctx.note(
        'start accepted',
        start.exit === 0 && /Starting service/.test(start.output) && !/Error:/.test(start.output),
        start.output
          .trim()
          .split('\n')
          .filter(l => /Starting service|Error:/.test(l))
          .join(' ')
          .slice(0, 160)
      )
      sleepMs(8000)
      ctx.note('service running', serviceRunning(), 'dumpsys activity services')
      ctx.note('flag persisted true', prefsFlag() === 'true', `flag=${prefsFlag()}`)
      const f = fatals()
      ctx.note('no app FATAL', f.count === 0, `fatal=${f.count}`)
      fs.writeFileSync(path.join(LOG_DIR, 'U9-1-logcat.log'), lib.redactLog(f.log))
    })

  if (want('U9-2'))
    check('U9-2', 'process death keeps the flag (durable)', 'REAL-EMULATOR', ctx => {
      sh(['am force-stop ' + PKG], 30000)
      sleepMs(5000)
      const pid = sh(['pidof ' + PKG], 30000).raw.trim()
      ctx.note('process dead after force-stop', pid === '', `pid=${pid || '(none)'}`)
      ctx.note('flag still true after death', prefsFlag() === 'true', `flag=${prefsFlag()}`)
      ctx.note('relaunch clears stopped state', relaunchApp(), 'pre-reboot precondition')
    })

  if (want('U9-3'))
    check('U9-3', 'reboot auto-restarts service iff flag set', 'REAL-EMULATOR', ctx => {
      adb(['reboot'], 60000)
      sleepMs(30000)
      const waited = waitBoot('reboot')
      ctx.note('reboot completed', true, `waited=${waited}s`)
      ctx.note('service auto-restarted after reboot', pollServiceRunning(90), 'dumpsys activity services (polled 90s)')
      ctx.note('flag still true', prefsFlag() === 'true', `flag=${prefsFlag()}`)
      const f = fatals()
      ctx.note('no app FATAL across reboot', f.count === 0, `fatal=${f.count}`)
      fs.writeFileSync(path.join(LOG_DIR, 'U9-3-logcat.log'), lib.redactLog(f.log))
    })

  if (want('U9-4'))
    check('U9-4', 'no flag → no restart (fail closed)', 'REAL-EMULATOR', ctx => {
      sh(['run-as ' + PKG + ' rm -f ' + PREFS], 30000)
      sh(['am force-stop ' + PKG], 30000)
      ctx.note('flag cleared', prefsFlag() === 'absent', `flag=${prefsFlag()}`)
      ctx.note(
        'relaunch clears stopped state (negative is meaningful)',
        relaunchApp(),
        'receiver will fire and must refuse'
      )
      adb(['reboot'], 60000)
      sleepMs(30000)
      const waited = waitBoot('reboot-2')
      ctx.note('second reboot completed', true, `waited=${waited}s`)
      sleepMs(20000)
      ctx.note('service NOT restarted without flag', !serviceRunning(), 'dumpsys activity services')
      const f = fatals()
      ctx.note('no app FATAL', f.count === 0, `fatal=${f.count}`)
      fs.writeFileSync(path.join(LOG_DIR, 'U9-4-logcat.log'), lib.redactLog(f.log))
    })

  if (want('U9-5'))
    check('U9-5', 'START_STICKY recreates service after kill (null-intent path)', 'REAL-EMULATOR', ctx => {
      sh(['pm grant ' + PKG + ' android.permission.BLUETOOTH_CONNECT'], 30000)
      const start = startService()
      ctx.note('service (re)started', start.exit === 0 && !/Error:/.test(start.raw || ''), 'restartSticky=true')
      sleepMs(8000)
      ctx.note('flag true', prefsFlag() === 'true', `flag=${prefsFlag()}`)
      sh(['am kill ' + PKG], 60000)
      ctx.note('service recreated after kill', pollServiceRunning(120), 'dumpsys activity services (polled 120s)')
      ctx.note('flag still true', prefsFlag() === 'true', `flag=${prefsFlag()}`)
      const f = fatals()
      ctx.note('no app FATAL', f.count === 0, `fatal=${f.count}`)
      fs.writeFileSync(path.join(LOG_DIR, 'U9-5-logcat.log'), lib.redactLog(f.log))
    })

  const summary = {
    tests: records.length,
    assertionsPassed: records.reduce((n, r) => n + r.assertions.passed, 0),
    assertionsTotal: records.reduce((n, r) => n + r.assertions.total, 0),
    byResult: records.reduce((m, r) => {
      m[r.result] = (m[r.result] || 0) + 1
      return m
    }, {})
  }
  fs.writeFileSync(
    RESULTS,
    JSON.stringify({ battery: 'u9', avd: AVD, serial: SERIAL, summary, records }, null, 2) + '\n'
  )
  console.log(
    `u9 battery: ${summary.tests} tests, ${summary.assertionsPassed}/${summary.assertionsTotal} assertions, ${JSON.stringify(summary.byResult)}`
  )
  console.log(`results: emulator-probe/u9/results-u9.json`)
  if (!noCleanup) {
    run('adb', ['-s', SERIAL, 'emu', 'kill'], 30000)
    sleepMs(5)
    run('rm', ['-rf', path.join(AVD_HOME, `${AVD}.avd`), path.join(AVD_HOME, `${AVD}.ini`)], 60000)
    console.log('cleanup: owned emulator + AVD removed')
  } else console.log('--no-cleanup: emulator left on emulator-5556')
}

main()
