const fs = require('fs')
const path = require('path')

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
