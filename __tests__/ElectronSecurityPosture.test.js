const fs = require('node:fs')
const path = require('node:path')

const root = path.resolve(__dirname, '..')

function source(relativePath) {
  return fs.readFileSync(path.join(root, relativePath), 'utf8')
}

describe('Electron secure host posture', () => {
  test('the documented composition requires a sandboxed renderer and proves that requirement', () => {
    const main = source('example-electron/composition-main.js')
    const composition = source('example-electron/composition.js')

    expect(main).toContain('contextIsolation: true')
    expect(main).toContain('nodeIntegration: false')
    expect(main).toContain('sandbox: true')
    expect(composition).toContain('assert.equal(main.browserWindowWebPreferences.sandbox, true)')
  })

  test('the renderer-facing graph contains no native-addon authority or generic IPC bridge', () => {
    const renderer = source('src/electron-renderer.ts')
    const preload = source('example-electron/composition-preload.js')

    expect(renderer).not.toContain('desktop-core-addon')
    expect(renderer).not.toContain('native/desktop-core')
    expect(preload).not.toContain('require(')
    expect(preload).not.toMatch(/\bipcRenderer\s*\.\s*(?:invoke|send|on|removeListener)\b/)
    expect(preload).toContain("'invoke'")
    expect(preload).toContain("'subscribe'")
    expect(preload).toContain("'acknowledge'")
  })

  test('the security model treats renderer sandboxing as a requirement, not an optional preference', () => {
    const securityModel = source('docs/ELECTRON_SECURITY_MODEL.md')
    const guide = source('docs/ELECTRON.md')

    expect(securityModel).toContain('`sandbox: true`')
    expect(securityModel).toContain('`app.enableSandbox()`')
    expect(securityModel).not.toContain('sandbox where the host allows it')
    expect(guide).toContain('`sandbox: true`')
    expect(guide).toContain('Node-API binary')
  })
})
