const fs = require('node:fs')
const os = require('node:os')
const path = require('node:path')
const { spawnSync } = require('node:child_process')

test('the canonical UniFFI owner rejects a generated binding mismatch before exchange', () => {
  const owner = fs.readFileSync(path.resolve(__dirname, '../../bindings/uniffi/run_uniffi_roundtrip.sh'), 'utf8')
  const block = owner.slice(
    owner.indexOf('diff -r generated/kotlin'),
    owner.indexOf('echo "--- uniffi: Python exchange')
  )
  const temporary = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-uniffi-repro-'))
  try {
    for (const language of ['kotlin', 'swift', 'python']) {
      for (const directory of ['generated', 'regen'])
        fs.mkdirSync(path.join(temporary, directory, language), { recursive: true })
      fs.writeFileSync(path.join(temporary, 'generated', language, 'binding'), 'committed')
      fs.writeFileSync(path.join(temporary, 'regen', language, 'binding'), 'different')
    }
    const result = spawnSync('sh', ['-c', `set -eu\nREGEN=regen\n${block}\necho reached-exchange`], {
      cwd: temporary,
      encoding: 'utf8'
    })
    expect(result.status).not.toBe(0)
    expect(result.stdout).not.toContain('reached-exchange')
  } finally {
    fs.rmSync(temporary, { recursive: true, force: true })
  }
})
