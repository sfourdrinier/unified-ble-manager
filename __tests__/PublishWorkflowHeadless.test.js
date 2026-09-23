const fs = require('fs')
const path = require('path')

test('tag publication runs its Linux Electron prebuild smoke with a display server', () => {
  const workflow = fs.readFileSync(path.join(__dirname, '../.github/workflows/publish.yml'), 'utf8')
  expect(workflow).toContain('sudo apt-get install --no-install-recommends -y xvfb xauth')
  expect(workflow).toContain('xvfb-run -a ./node_modules/.bin/electron --no-sandbox scripts/ci/electron-main-smoke.js')
})
