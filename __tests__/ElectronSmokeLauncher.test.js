const fs = require('fs')
const os = require('os')
const path = require('path')
const { spawnSync } = require('child_process')

const launcher = path.resolve(__dirname, '../scripts/ci/run-electron-main-smoke.sh')

test.each(['Linux', 'Darwin', 'MINGW64_NT'])('Electron smoke on %s preserves the real command and exit', platform => {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-electron-launcher-'))
  try {
    const bin = path.join(directory, 'bin')
    const electronBin = path.join(directory, 'node_modules/.bin')
    fs.mkdirSync(bin)
    fs.mkdirSync(electronBin, { recursive: true })
    fs.writeFileSync(path.join(bin, 'uname'), '#!/bin/sh\nprintf "%s\\n" "$SMOKE_PLATFORM"\n', { mode: 0o755 })
    fs.writeFileSync(
      path.join(bin, 'xvfb-run'),
      '#!/bin/sh\nprintf "%s\\n" "$@" > "$DISPLAY_LOG"\nshift\nexec "$@"\n',
      { mode: 0o755 }
    )
    fs.writeFileSync(
      path.join(electronBin, 'electron'),
      '#!/bin/sh\nprintf "%s\\n" "$@" > "$SMOKE_LOG"\nexit "$SMOKE_EXIT"\n',
      { mode: 0o755 }
    )
    for (const exit of ['0', '19']) {
      const result = spawnSync('bash', [launcher], {
        cwd: directory,
        encoding: 'utf8',
        env: {
          ...process.env,
          PATH: `${bin}${path.delimiter}${process.env.PATH}`,
          SMOKE_PLATFORM: platform,
          SMOKE_EXIT: exit,
          SMOKE_LOG: path.join(directory, 'smoke.log'),
          DISPLAY_LOG: path.join(directory, 'display.log')
        }
      })
      expect(result.error).toBeUndefined()
      expect(result.status).toBe(Number(exit))
      expect(result.stderr).toBe('')
      expect(fs.readFileSync(path.join(directory, 'smoke.log'), 'utf8').trim().split('\n')).toEqual(
        platform === 'Linux'
          ? ['--no-sandbox', 'scripts/ci/electron-main-smoke.js']
          : ['scripts/ci/electron-main-smoke.js']
      )
      if (platform === 'Linux') {
        expect(fs.readFileSync(path.join(directory, 'display.log'), 'utf8').trim().split('\n')).toEqual([
          '-a',
          './node_modules/.bin/electron',
          '--no-sandbox',
          'scripts/ci/electron-main-smoke.js'
        ])
      } else {
        expect(fs.existsSync(path.join(directory, 'display.log'))).toBe(false)
      }
    }
  } finally {
    fs.rmSync(directory, { recursive: true, force: true })
  }
})
