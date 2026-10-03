const fs = require('fs')
const path = require('path')

test.each(['publish.yml', 'apple-ci.yml'])('Apple tooling in %s does not install Intel-only targets', filename => {
  const workflow = fs.readFileSync(path.join(__dirname, '../.github/workflows', filename), 'utf8')
  expect(workflow).not.toContain('x86_64-apple-ios')
  expect(workflow).not.toContain('x86_64-apple-darwin')
  expect(workflow).not.toContain('x86_64-sim')
  const setupAction = fs.readFileSync(path.join(__dirname, '../.github/actions/setup-apple-rust/action.yml'), 'utf8')
  expect(workflow).toContain('uses: ./.github/actions/setup-apple-rust')
  expect(setupAction).toContain('...BINDINGS.uniffi.targets')
  expect(setupAction).toContain("execFileSync('rustup', ['toolchain', 'install', pin[1], '--profile', 'minimal']")
  expect(setupAction.indexOf("['toolchain', 'install'")).toBeLessThan(setupAction.indexOf("['target', 'add'"))
  const { BINDINGS } = require('../scripts/release/native-build-identity')
  expect(BINDINGS.uniffi.targets).toContain('aarch64-apple-ios-sim')
  expect(BINDINGS.uniffi.targets).toContain('aarch64-apple-ios')
  expect(BINDINGS.uniffi.targets.every(target => target.startsWith('aarch64-'))).toBe(true)
})

test('both generic iOS consumer builds request only the maintained arm64 simulator architecture', () => {
  const workflow = fs.readFileSync(path.join(__dirname, '../.github/workflows/apple-ci.yml'), 'utf8')
  const builds = [...workflow.matchAll(/xcodebuild \\\n[\s\S]*?\n\s+build/g)]
  expect(builds).toHaveLength(2)
  for (const [command] of builds) {
    expect(command).toContain("-destination 'generic/platform=iOS Simulator'")
    expect(command).toMatch(/^\s+ARCHS=arm64 \\\s*$/m)
  }
})

test('tag publication runs its Linux Electron prebuild smoke with a display server', () => {
  const workflow = fs.readFileSync(path.join(__dirname, '../.github/workflows/publish.yml'), 'utf8')
  const installer = fs.readFileSync(
    path.join(__dirname, '../scripts/ci/install-linux-native-system-dependencies.sh'),
    'utf8'
  )

  expect(workflow).toContain('bash scripts/ci/install-linux-native-system-dependencies.sh desktop-prebuild')
  expect(installer.split(/\r?\n/).filter(line => line.trim() === 'xvfb')).toHaveLength(1)
  expect(installer.split(/\r?\n/).filter(line => line.trim() === 'xauth')).toHaveLength(1)
  const launcher = fs.readFileSync(path.join(__dirname, '../scripts/ci/run-electron-main-smoke.sh'), 'utf8')
  const ci = fs.readFileSync(path.join(__dirname, '../.github/workflows/ci.yml'), 'utf8')
  expect(workflow).toContain('bash scripts/ci/run-electron-main-smoke.sh')
  expect(ci).toContain('bash scripts/ci/run-electron-main-smoke.sh')
  expect(ci).toContain('bash scripts/ci/install-linux-native-system-dependencies.sh tauri-electron')
  expect(ci).toContain('bash scripts/ci/install-linux-native-system-dependencies.sh desktop-prebuild')
  expect(launcher).toContain(
    'exec xvfb-run -a ./node_modules/.bin/electron --no-sandbox scripts/ci/electron-main-smoke.js'
  )
  expect(launcher).toContain('exec ./node_modules/.bin/electron scripts/ci/electron-main-smoke.js')
  expect(installer).toContain('"${bluez_packages[@]}" "${tauri_packages[@]}" "${electron_smoke_packages[@]}"')
})
