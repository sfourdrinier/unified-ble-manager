'use strict'

const fs = require('node:fs')
const os = require('node:os')
const path = require('node:path')
const { runNpmPack, withCleanup } = require('../scripts/ci/pack-fixture-process')

test.each([false, true])('explicit binding pack policies retain sources with polluted parent=%s', polluted => {
  const fixture = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-binding-pack-exclusions-'))
  const root = path.join(__dirname, '..')
  const write = (file, contents) => {
    const target = path.join(fixture, file)
    fs.mkdirSync(path.dirname(target), { recursive: true })
    fs.writeFileSync(target, contents)
  }
  withCleanup(
    () => {
      write(
        'package.json',
        JSON.stringify({ name: 'binding-exclusion-fixture', version: '1.0.0', files: ['bindings', 'native'] })
      )
      const policy = text => text.split('\n').filter(line => line.trim() !== '' && !line.startsWith('#'))
      for (const binding of ['jni', 'napi', 'uniffi', 'wasm']) {
        const directory = `bindings/${binding}`
        const gitPolicy = fs.readFileSync(path.join(root, directory, '.gitignore'), 'utf8')
        const npmPolicy = fs.readFileSync(path.join(root, directory, '.npmignore'), 'utf8')
        expect(policy(npmPolicy)).toEqual(policy(gitPolicy))
        write(`${directory}/.gitignore`, gitPolicy)
        write(`${directory}/.npmignore`, npmPolicy)
        write(`${directory}/src/lib.rs`, '// required source\n')
        write(`${directory}/target/build-output`, 'excluded')
      }
      write('bindings/napi/debug.node', 'excluded')
      write('bindings/wasm/debug.wasm', 'excluded')
      write('native/desktop-core/prebuilds/linux-x64/ubm_desktop_core.node', 'required release binary')
      write('native/other-binding/runtime.wasm', 'non-feasibility artifact remains included')
      const parentEnvironment = polluted
        ? {
            ...process.env,
            npm_config_verify_deps_before_run: 'false',
            NPM_CONFIG_PATCHED_DEPENDENCIES: '{}',
            npm_config__jsr_registry: 'https://jsr.io/'
          }
        : process.env
      // This dependency-free pack fixture owns its package configuration. pnpm
      // lifecycle variables are not npm configuration and must not leak into it.
      const environment = Object.fromEntries(
        Object.entries(parentEnvironment).filter(([key]) => !/^npm_config_/i.test(key))
      )
      // Direct Node invocation removes the cmd.exe descendant/cwd-lock hazard.
      // The masked CI error does not prove that hazard caused its observed lock.
      // No lock retry guesses or hidden cleanup errors.
      const result = runNpmPack(fixture, environment)
      expect(result.error).toBeUndefined()
      expect(result.status).toBe(0)
      expect(result.stderr).toBe('')
      const packed = JSON.parse(result.stdout)[0].files.map(file => file.path)
      expect(packed).toEqual([
        'bindings/jni/src/lib.rs',
        'bindings/napi/src/lib.rs',
        'bindings/uniffi/src/lib.rs',
        'bindings/wasm/src/lib.rs',
        'native/desktop-core/prebuilds/linux-x64/ubm_desktop_core.node',
        'native/other-binding/runtime.wasm',
        'package.json'
      ])
    },
    () => {
      fs.rmSync(fixture, { recursive: true, force: true })
    }
  )
})
