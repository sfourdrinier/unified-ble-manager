const fs = require('fs')
const os = require('os')
const path = require('path')
const { spawnSync } = require('child_process')

const launcher = path.resolve(__dirname, '../scripts/ci/run-electron-main-smoke.sh')
const verifyRelease = path.resolve(__dirname, '../scripts/verify-release.sh')

test.each(
  ['Linux', 'Darwin', 'MINGW64_NT'].flatMap(platform =>
    [false, true].flatMap(bootstrapPath =>
      [false, true].map(releaseInvocation => ({ platform, bootstrapPath, releaseInvocation }))
    )
  )
)(
  'Electron smoke on $platform preserves command and exit (PATH bootstrap: $bootstrapPath, release invocation: $releaseInvocation)',
  ({ platform, bootstrapPath, releaseInvocation }) => {
    const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-electron-launcher-'))
    try {
      const bin = path.join(directory, 'bin')
      const electronBin = path.join(directory, 'node_modules/.bin')
      fs.mkdirSync(bin)
      fs.mkdirSync(electronBin, { recursive: true })
      fs.mkdirSync(path.join(directory, 'scripts/ci'), { recursive: true })
      fs.copyFileSync(launcher, path.join(directory, 'scripts/ci/run-electron-main-smoke.sh'))
      // Git Bash establishes its own POSIX PATH after importing the Windows
      // environment. Install the mock search path inside the executing shell,
      // rather than depending on the parent's case-insensitive PATH conversion.
      fs.writeFileSync(path.join(directory, 'smoke-shell-env'), 'export PATH="$PWD/bin:$PATH"\n')
      fs.writeFileSync(path.join(bin, 'uname'), '#!/bin/sh\nprintf "%s\\n" "$SMOKE_PLATFORM"\n', { mode: 0o755 })
      fs.writeFileSync(
        path.join(bin, 'xvfb-run'),
        '#!/bin/sh\nprintf "%s\\n" "$@" > "$DISPLAY_LOG"\nshift\nexec "$@"\n',
        { mode: 0o755 }
      )
      fs.writeFileSync(
        path.join(electronBin, 'electron'),
        '#!/bin/sh\nprintf "%s\\n" "$@" > "$SMOKE_LOG"\nprintf "%s\\n" "$UBM_SMOKE_USE_SOURCE" > "$SOURCE_LOG"\nexit "$SMOKE_EXIT"\n',
        { mode: 0o755 }
      )
      for (const exit of ['0', '19']) {
        const invocation = releaseInvocation
          ? fs
              .readFileSync(verifyRelease, 'utf8')
              .split('\n')
              .find(line => line.startsWith('UBM_SMOKE_USE_SOURCE=1 '))
          : `bash "${launcher}"`
        expect(invocation).toBeDefined()
        const argumentsForShell = ['-c', `${bootstrapPath ? 'export PATH="/usr/bin:/bin:$PATH"; ' : ''}${invocation}`]
        const result = spawnSync('bash', argumentsForShell, {
          cwd: directory,
          encoding: 'utf8',
          env: {
            ...process.env,
            BASH_ENV: 'smoke-shell-env',
            PATH: `${bin}${path.delimiter}${process.env.PATH}`,
            SMOKE_PLATFORM: platform,
            SMOKE_EXIT: exit,
            UBM_SMOKE_USE_SOURCE: '0',
            SMOKE_LOG: path.join(directory, 'smoke.log'),
            SOURCE_LOG: path.join(directory, 'source.log'),
            DISPLAY_LOG: path.join(directory, 'display.log')
          }
        })
        expect(result.error).toBeUndefined()
        expect(result.status).toBe(Number(exit))
        expect(result.stderr).toBe('')
        expect(fs.readFileSync(path.join(directory, 'source.log'), 'utf8').trim()).toBe(releaseInvocation ? '1' : '0')
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
  }
)
