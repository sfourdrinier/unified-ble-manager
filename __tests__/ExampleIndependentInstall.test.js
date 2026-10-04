'use strict'

const fs = require('node:fs')
const os = require('node:os')
const path = require('node:path')
const { spawnSync } = require('node:child_process')
const packageJson = require('../package.json')

test.each([false, true])(
  'driver installs the independent example with its own autoInstallPeers=%s',
  autoInstallPeers => {
    const fixture = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-independent-example-install-'))
    const child = path.join(fixture, 'example-expo')
    const lock = peers =>
      `lockfileVersion: '9.0'\n\nsettings:\n  autoInstallPeers: ${peers}\n  excludeLinksFromLockfile: false\n\nimporters:\n\n  .: {}\n`
    const rootLock = lock(true)
    const childLock = lock(autoInstallPeers)
    try {
      fs.mkdirSync(child)
      fs.writeFileSync(
        path.join(fixture, 'package.json'),
        JSON.stringify({
          name: 'independent-example-parent',
          private: true,
          packageManager: packageJson.packageManager,
          scripts: { 'test:driver': packageJson.scripts['test:driver'] }
        })
      )
      fs.writeFileSync(
        path.join(fixture, 'pnpm-workspace.yaml'),
        'patchedDependencies:\n  documentation@14.0.3: patches/root-only.patch\n'
      )
      fs.writeFileSync(path.join(fixture, 'pnpm-lock.yaml'), rootLock)
      fs.writeFileSync(
        path.join(child, 'package.json'),
        JSON.stringify({
          name: 'independent-example-child',
          private: true,
          scripts: { 'test:driver': 'node -e "process.stdout.write(\'driver-ran\\n\')"' }
        })
      )
      fs.writeFileSync(path.join(child, '.npmrc'), `auto-install-peers=${autoInstallPeers}\n`)
      fs.writeFileSync(path.join(child, 'pnpm-lock.yaml'), childLock)
      const installer = path.join(__dirname, '../examples-shared/dev/install-example-dependencies.js')
      if (fs.existsSync(installer)) {
        const target = path.join(fixture, 'examples-shared/dev/install-example-dependencies.js')
        fs.mkdirSync(path.dirname(target), { recursive: true })
        fs.copyFileSync(installer, target)
      }
      const result = spawnSync(process.platform === 'win32' ? 'pnpm.cmd' : 'pnpm', ['test:driver'], {
        cwd: fixture,
        encoding: 'utf8',
        shell: process.platform === 'win32',
        timeout: 30000
      })
      expect(result.error).toBeUndefined()
      expect(result.stdout + result.stderr).toContain('driver-ran')
      expect(result.status).toBe(0)
      expect(fs.readFileSync(path.join(child, 'pnpm-lock.yaml'), 'utf8')).toBe(childLock)
      expect(fs.readFileSync(path.join(fixture, 'pnpm-lock.yaml'), 'utf8')).toBe(rootLock)
    } finally {
      fs.rmSync(fixture, { recursive: true, force: true })
    }
  }
)
