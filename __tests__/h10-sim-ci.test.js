const fs = require('node:fs')
const path = require('node:path')

test('the existing simulator lane tests every backend and its patched transport policy', () => {
  const workflow = fs.readFileSync(path.join(__dirname, '../.github/workflows/ci.yml'), 'utf8')
  const job = workflow.split('\n  h10-sim:\n')[1].split('\n  changes:\n')[0]
  expect(job).toContain('os: [ubuntu-latest, macos-latest, windows-latest]')
  expect(job).toContain('cargo test --manifest-path tool/h10-sim/Cargo.toml --locked -p ble-peripheral-rust --lib')
  expect(job).toContain('shell: bash')
  expect(job).not.toContain('/tmp/h10-driver-hello.json')
})
