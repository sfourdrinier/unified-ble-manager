const fs = require('fs')
const path = require('path')

test.each(['publish.yml', 'apple-ci.yml'])('Apple tooling in %s does not install Intel-only targets', filename => {
  const workflow = fs.readFileSync(path.join(__dirname, '../.github/workflows', filename), 'utf8')
  expect(workflow).not.toContain('x86_64-apple-ios')
  expect(workflow).not.toContain('x86_64-apple-darwin')
  expect(workflow).not.toContain('x86_64-sim')
  expect(workflow).toContain('aarch64-apple-ios-sim')
  expect(workflow).toContain('aarch64-apple-ios')
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
  expect(workflow).toContain('xvfb-run -a ./node_modules/.bin/electron --no-sandbox scripts/ci/electron-main-smoke.js')
})
