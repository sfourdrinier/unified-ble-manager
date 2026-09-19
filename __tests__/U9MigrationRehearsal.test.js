const crypto = require('crypto')
const fs = require('fs')
const os = require('os')
const path = require('path')

const root = path.join(__dirname, '..')
const PREFS_FILE = 'unified-ble-manager'
const PREFS_KEY = 'unified-ble-manager.background.session-intent-exists'

describe('U9 migration rehearsal: 4.x durable store honored on copies', () => {
  const fixture = path.join(__dirname, 'fixtures', 'u9-fourx-prefs.xml')

  test('4.x prefs fixture parses: session-intent flag true', () => {
    const xml = fs.readFileSync(fixture, 'utf8')
    expect(xml).toContain(`<boolean name="${PREFS_KEY}" value="true" />`)
  })

  test('rehearsal reads a copy; the original stays byte-identical', () => {
    const before = fs.readFileSync(fixture)
    const beforeHash = crypto.createHash('sha256').update(before).digest('hex')
    const workdir = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-u9-rehearsal-'))
    try {
      // The ONLY durable 4.x store is this one boolean. 5.0 reads the same
      // file/key/default (the restart path is unchanged — no schema
      // migration exists to run). Rehearse the read against a copy.
      const copy = path.join(workdir, `${PREFS_FILE}.xml`)
      fs.copyFileSync(fixture, copy)
      const rehearsed = fs.readFileSync(copy, 'utf8')
      const match = rehearsed.match(
        new RegExp(`<boolean name="${PREFS_KEY.replace(/[./-]/g, (c) => `\\${c}`)}" value="(true|false)" />`)
      )
      expect(match).not.toBeNull()
      expect(match[1]).toBe('true')
    } finally {
      fs.rmSync(workdir, { recursive: true, force: true })
    }
    const afterHash = crypto.createHash('sha256').update(fs.readFileSync(fixture)).digest('hex')
    expect(afterHash).toBe(beforeHash)
  })

  test('5.0 reader contract unchanged: same file, key, fail-closed default', () => {
    const service = fs.readFileSync(
      path.join(root, 'android/src/main/java/com/sfourdrinier/unifiedblemanager/BlePlxForegroundService.java'),
      'utf8'
    )
    const driver = fs.readFileSync(
      path.join(
        root,
        'android/src/main/java/com/sfourdrinier/unifiedblemanager/background/AndroidConnectedDeviceForegroundServiceDriver.java'
      ),
      'utf8'
    )
    const receiver = fs.readFileSync(
      path.join(
        root,
        'android/src/main/java/com/sfourdrinier/unifiedblemanager/background/BlePlxForegroundServiceRecoveryReceiver.java'
      ),
      'utf8'
    )
    // Key constant value.
    expect(service).toContain(`"${PREFS_KEY}"`)
    // Every read site: same file, same key, default-false (fail closed —
    // no flag, no restart).
    for (const [name, source] of [['service', service], ['driver', driver], ['receiver', receiver]]) {
      expect(source).toContain(`getSharedPreferences("${PREFS_FILE}"`)
      expect(source).toContain('SESSION_INTENT_PREFERENCE, false')
    }
    // Writes are synchronous commits (durable before the service reports
    // started), never fire-and-forget applies.
    expect(service).toContain('.putBoolean(SESSION_INTENT_PREFERENCE')
    expect(service.match(/\.commit\(\)/g).length).toBeGreaterThanOrEqual(2)
    expect(receiver).not.toContain('.putBoolean(')
  })
})
