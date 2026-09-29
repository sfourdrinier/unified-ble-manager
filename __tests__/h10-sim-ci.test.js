const fs = require('node:fs')
const path = require('node:path')

test('simulator cross-check commands resolve from their documented working directories', () => {
  const root = path.resolve(__dirname, '..')
  const readme = fs.readFileSync(path.join(root, 'tool/h10-sim/README.md'), 'utf8')
  const block = readme.split('## Build\n')[1].match(/```sh\n([\s\S]*?)```/)[1]
  let cwd = root
  let crosschecks = 0
  for (const line of block.trim().split('\n')) {
    if (line.startsWith('cd ')) cwd = path.resolve(cwd, line.slice(3).trim())
    if (line.startsWith('node ')) {
      const script = line.split(/\s+/)[1]
      expect(fs.existsSync(path.resolve(cwd, script))).toBe(true)
      expect(path.resolve(cwd, script)).toBe(path.join(root, 'tool/h10-sim/tests/xcheck/run-xcheck.cjs'))
      expect(line).toContain('from tool/h10-sim')
      crosschecks++
    }
  }
  expect(crosschecks).toBe(1)
  expect(readme).toContain('`node tool/h10-sim/tests/xcheck/run-xcheck.cjs` (repo root)')
  expect(readme).not.toContain('`node tests/xcheck/run-xcheck.cjs` (repo root)')
})

test('simulator documentation distinguishes BlueZ disconnect policy from RF loss', () => {
  const readme = fs.readFileSync(path.join(__dirname, '../tool/h10-sim/README.md'), 'utf8')
  expect(readme).toContain('For non-trusted LE peers, `Device1.Disconnect` disables incoming connections')
  expect(readme).toContain('until `Device1.Connect` is called again')
  expect(readme).toContain('not an RF supervision-timeout simulation')
  expect(readme).not.toContain('reconnect at once')
})

test('the existing simulator lane tests every backend and its patched transport policy', () => {
  const workflow = fs.readFileSync(path.join(__dirname, '../.github/workflows/ci.yml'), 'utf8')
  const job = workflow.split('\n  h10-sim:\n')[1].split('\n  changes:\n')[0]
  expect(job).toContain('os: [ubuntu-latest, macos-latest, windows-latest]')
  expect(job).toContain('cargo test --manifest-path tool/h10-sim/Cargo.toml --locked -p ble-peripheral-rust --lib')
  expect(job).toContain('shell: bash')
  expect(job).not.toContain('/tmp/h10-driver-hello.json')
})

test('the Linux simulator lane installs the private D-Bus regression runtime through the shared profile', () => {
  const workflow = fs.readFileSync(path.join(__dirname, '../.github/workflows/ci.yml'), 'utf8')
  const job = workflow.split('\n  h10-sim:\n')[1].split('\n  changes:\n')[0]
  const installer = fs.readFileSync(
    path.join(__dirname, '../scripts/ci/install-linux-native-system-dependencies.sh'),
    'utf8'
  )
  const bluezPackages = installer
    .match(/readonly -a bluez_packages=\(([\s\S]*?)\)/)?.[1]
    .trim()
    .split(/\s+/)
  expect(job).toContain('bash scripts/ci/install-linux-native-system-dependencies.sh bluez')
  expect(bluezPackages).toEqual(expect.arrayContaining(['libdbus-1-dev', 'pkg-config', 'dbus-daemon']))
})
