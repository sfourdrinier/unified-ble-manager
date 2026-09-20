'use strict'

// GTV1: example-expo/scripts/android-tv-emu.sh makes the Google TV emulator a
// repeatable test host (boot, install the phone APK as-is, reverse the driver
// and Metro ports, launch). The phone APK installs and runs on the TV
// emulator unmodified, so the script never forks the example; it only drives
// adb/emulator with pinned arguments.
//
// ADB and ANDROID_EMULATOR_BIN redirect to fake shims so these tests never
// touch a real device.

const { execFileSync } = require('node:child_process')
const fs = require('node:fs')
const os = require('node:os')
const path = require('node:path')

const SCRIPT = path.join(__dirname, '..', 'example-expo', 'scripts', 'android-tv-emu.sh')

function run(args, env) {
  try {
    const stdout = execFileSync('bash', [SCRIPT, ...args], {
      env: { ...process.env, ...env },
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'pipe']
    })
    return { exit: 0, stdout, stderr: '' }
  } catch (error) {
    return { exit: error.status ?? 1, stdout: error.stdout ?? '', stderr: error.stderr ?? '' }
  }
}

// A fake adb that records every invocation and answers boot polls as booted.
function fakeAdb(dir) {
  const log = path.join(dir, 'adb.log')
  const bin = path.join(dir, 'adb')
  fs.writeFileSync(
    bin,
    `#!/usr/bin/env bash\necho "$@" >> "${log}"\n` +
      `if [[ "$*" == *"getprop sys.boot_completed"* ]]; then echo 1; fi\nexit 0\n`
  )
  fs.chmodSync(bin, 0o755)
  return { bin, log }
}

function fakeEmulator(dir) {
  const log = path.join(dir, 'emu.log')
  const bin = path.join(dir, 'emulator')
  fs.writeFileSync(bin, `#!/usr/bin/env bash\necho "$@" >> "${log}"\nexit 0\n`)
  fs.chmodSync(bin, 0o755)
  return { bin, log }
}

function tmpDir() {
  return fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-android-tv-'))
}

describe('android-tv-emu.sh', () => {
  test('reverse mirrors the proven phone port pair against the TV serial', () => {
    const dir = tmpDir()
    try {
      const adb = fakeAdb(dir)
      const out = run(['reverse'], { ADB: adb.bin, ANDROID_TV_SERIAL: 'emulator-5554' })
      expect(out.exit).toBe(0)
      const calls = fs.readFileSync(adb.log, 'utf8')
      expect(calls).toContain('-s emulator-5554 reverse tcp:8795 tcp:8795')
      expect(calls).toContain('-s emulator-5554 reverse tcp:8081 tcp:8082')
    } finally {
      fs.rmSync(dir, { recursive: true, force: true })
    }
  })

  test('reverse honours serial and port overrides', () => {
    const dir = tmpDir()
    try {
      const adb = fakeAdb(dir)
      const out = run(['reverse'], {
        ADB: adb.bin,
        ANDROID_TV_SERIAL: 'emulator-5556',
        ANDROID_TV_METRO_PORT: '8091',
        ANDROID_TV_DRIVER_PORT: '8796'
      })
      expect(out.exit).toBe(0)
      const calls = fs.readFileSync(adb.log, 'utf8')
      expect(calls).toContain('-s emulator-5556 reverse tcp:8796 tcp:8796')
      expect(calls).toContain('-s emulator-5556 reverse tcp:8081 tcp:8091')
    } finally {
      fs.rmSync(dir, { recursive: true, force: true })
    }
  })

  test('install fails loudly when the APK is missing and uses install -r otherwise', () => {
    const dir = tmpDir()
    try {
      const adb = fakeAdb(dir)
      const missing = run(['install'], { ADB: adb.bin, ANDROID_TV_APK: path.join(dir, 'no.apk') })
      expect(missing.exit).not.toBe(0)
      expect(missing.stderr).toMatch(/apk/i)
      const apk = path.join(dir, 'app-debug.apk')
      fs.writeFileSync(apk, 'fake-apk')
      const ok = run(['install'], {
        ADB: adb.bin,
        ANDROID_TV_APK: apk,
        ANDROID_TV_SERIAL: 'emulator-5554'
      })
      expect(ok.exit).toBe(0)
      expect(fs.readFileSync(adb.log, 'utf8')).toContain(
        `-s emulator-5554 install -r ${apk}`
      )
    } finally {
      fs.rmSync(dir, { recursive: true, force: true })
    }
  })

  test('launch starts the example MainActivity and honours the package override', () => {
    const dir = tmpDir()
    try {
      const adb = fakeAdb(dir)
      const out = run(['launch'], { ADB: adb.bin, ANDROID_TV_SERIAL: 'emulator-5554' })
      expect(out.exit).toBe(0)
      expect(fs.readFileSync(adb.log, 'utf8')).toContain(
        '-s emulator-5554 shell am start -n com.sfourdrinier.bleplxexample/.MainActivity'
      )
      fs.truncateSync(adb.log, 0)
      const custom = run(['launch'], {
        ADB: adb.bin,
        ANDROID_TV_SERIAL: 'emulator-5554',
        ANDROID_TV_PACKAGE: 'com.example.tv'
      })
      expect(custom.exit).toBe(0)
      expect(fs.readFileSync(adb.log, 'utf8')).toContain(
        '-s emulator-5554 shell am start -n com.example.tv/.MainActivity'
      )
    } finally {
      fs.rmSync(dir, { recursive: true, force: true })
    }
  })

  test('boot starts the pinned AVD and prints the serial', () => {
    const dir = tmpDir()
    try {
      // First boot poll answers not-booted so boot starts the emulator;
      // later polls answer booted so the wait ends.
      const adbLog = path.join(dir, 'adb.log')
      const adbBin = path.join(dir, 'adb')
      const polls = path.join(dir, 'polls')
      fs.writeFileSync(
        adbBin,
        `#!/usr/bin/env bash\necho "$@" >> "${adbLog}"\n` +
          `if [[ "$*" == *"getprop sys.boot_completed"* ]]; then\n` +
          `  n=$(cat "${polls}" 2>/dev/null || echo 0); echo $((n + 1)) > "${polls}"\n` +
          `  if [[ "$n" == "0" ]]; then echo 0; else echo 1; fi\nfi\nexit 0\n`
      )
      fs.chmodSync(adbBin, 0o755)
      const adb = { bin: adbBin, log: adbLog }
      const emu = fakeEmulator(dir)
      const out = run(['boot'], {
        ADB: adb.bin,
        ANDROID_EMULATOR_BIN: emu.bin,
        ANDROID_TV_AVD: 'TV_TEST_AVD',
        ANDROID_TV_SERIAL: 'emulator-5554',
        ANDROID_TV_EMU_LOG: path.join(dir, 'emu-out.log'),
        ANDROID_TV_BOOT_TIMEOUT: '5'
      })
      expect(out.exit).toBe(0)
      // The emulator is spawned asynchronously (`&`): poll briefly for the
      // shim's arg log instead of assuming it won the race with our read.
      let emuArgs = null
      const deadline = Date.now() + 5000
      while (Date.now() < deadline) {
        try {
          emuArgs = fs.readFileSync(emu.log, 'utf8')
          break
        } catch {
          execFileSync('sleep', ['0.1'])
        }
      }
      expect(emuArgs).not.toBeNull()
      expect(emuArgs).toContain('-avd TV_TEST_AVD')
      expect(emuArgs).toContain('-no-snapshot')
      expect(emuArgs).toContain('-no-boot-anim')
      expect(out.stdout).toContain('emulator-5554')
    } finally {
      fs.rmSync(dir, { recursive: true, force: true })
    }
  })

  test('boot is a no-op when the emulator is already booted', () => {
    const dir = tmpDir()
    try {
      const adb = fakeAdb(dir)
      const emu = fakeEmulator(dir)
      const out = run(['boot'], {
        ADB: adb.bin,
        ANDROID_EMULATOR_BIN: emu.bin,
        ANDROID_TV_SERIAL: 'emulator-5554',
        ANDROID_TV_EMU_LOG: path.join(dir, 'emu-out.log')
      })
      expect(out.exit).toBe(0)
      expect(out.stdout).toMatch(/already booted/)
      expect(fs.existsSync(emu.log)).toBe(false)
    } finally {
      fs.rmSync(dir, { recursive: true, force: true })
    }
  })

  test('boot fails loudly without an emulator binary', () => {
    const dir = tmpDir()
    try {
      const adb = fakeAdb(dir)
      const out = run(['boot'], {
        ADB: adb.bin,
        ANDROID_EMULATOR_BIN: path.join(dir, 'no-emulator'),
        PATH: '/usr/bin:/bin'
      })
      expect(out.exit).not.toBe(0)
      expect(out.stderr).toMatch(/emulator/i)
    } finally {
      fs.rmSync(dir, { recursive: true, force: true })
    }
  })
})
