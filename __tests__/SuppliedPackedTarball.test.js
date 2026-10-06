const fs = require('node:fs')
const os = require('node:os')
const path = require('node:path')
const crypto = require('node:crypto')
const { suppliedPackedTarball } = require('../scripts/ci/supplied-packed-tarball')

test('ordinary production consumer helpers keep the original pack path', () => {
  expect(suppliedPackedTarball({})).toBeNull()
})

test('supplied artifact fails closed on absent digest, relative path, missing file or changed bytes', () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-supplied-tarball-test-'))
  try {
    const file = path.join(dir, 'canonical.tgz')
    fs.writeFileSync(file, 'candidate')
    const digest = crypto.createHash('sha256').update('candidate').digest('hex')
    expect(suppliedPackedTarball({ UBM_PACKED_TARBALL: file, UBM_PACKED_TARBALL_SHA256: digest })).toBe(file)
    expect(() => suppliedPackedTarball({ UBM_PACKED_TARBALL: file })).toThrow(/digest/)
    expect(() => suppliedPackedTarball({ UBM_PACKED_TARBALL_SHA256: digest })).toThrow(/path/)
    expect(() => suppliedPackedTarball({ UBM_PACKED_TARBALL: 'relative.tgz', UBM_PACKED_TARBALL_SHA256: digest })).toThrow(/absolute/)
    expect(() => suppliedPackedTarball({ UBM_PACKED_TARBALL: path.join(dir, 'absent'), UBM_PACKED_TARBALL_SHA256: digest })).toThrow()
    fs.writeFileSync(file, 'changed')
    expect(() => suppliedPackedTarball({ UBM_PACKED_TARBALL: file, UBM_PACKED_TARBALL_SHA256: digest })).toThrow(/mismatch/)
  } finally {
    fs.rmSync(dir, { recursive: true, force: true })
  }
})

test.each(['pack-install-smoke.js', 'tauri-packed-consumer-check.js', 'packed-host-consumer-check.js'])('%s accepts the sealed candidate without repacking', filename => {
  const source = fs.readFileSync(path.join(__dirname, '../scripts/ci', filename), 'utf8')
  expect(source).toContain('suppliedPackedTarball')
  expect(source).toContain('if (!suppliedTarball)')
})
