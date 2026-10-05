const fs = require('fs')
const path = require('path')
const { gitBashExecutableTemp, spawnFixtureShellSync } = require('./helpers/git-bash-temp')

const builder = fs.readFileSync(path.join(__dirname, '../android/build-rust-cdylib.sh'), 'utf8')

function fixturePathEnvironment(bin, environment, delimiter = path.delimiter) {
  const result = { ...environment }
  const pathKeys = Object.keys(result).filter(key => key.toLowerCase() === 'path')
  const inherited = pathKeys.map(key => result[key]).find(value => typeof value === 'string') ?? ''
  for (const key of pathKeys) delete result[key]
  result.PATH = [bin, inherited].filter(Boolean).join(delimiter)
  return result
}

// Git Bash accepts drive-qualified forward-slash paths; JavaScript's native
// Windows separators are not portable shell/shebang syntax.
const shellPath = file => file.split(path.sep).join('/')

describe('Android canonical C dependency toolchain', () => {
  let fixture
  beforeEach(() => {
    // Use the same executable volume as the Windows TV shell fixtures. The
    // runner's native TEMP need not be Git Bash's executable /tmp volume.
    fixture = fs.mkdtempSync(path.join(gitBashExecutableTemp(), 'ubm android c-'))
  })
  afterEach(() => fs.rmSync(fixture, { recursive: true, force: true }))

  function write(relative, content, executable = false) {
    const file = path.join(fixture, relative)
    fs.mkdirSync(path.dirname(file), { recursive: true })
    fs.writeFileSync(file, content, { mode: executable ? 0o755 : 0o644 })
    return file
  }

  function run(host, abi, target, archiver = true) {
    const hostTag = host === 'Darwin' ? 'darwin-x86_64' : 'linux-x86_64'
    const tools = `ndk/toolchains/llvm/prebuilt/${hostTag}/bin`
    const compilerTriple = target === 'armv7-linux-androideabi' ? 'armv7a-linux-androideabi' : target
    const compiler = shellPath(write(`${tools}/${compilerTriple}26-clang`, '#!/bin/sh\nexit 0\n', true))
    const ar = shellPath(path.join(fixture, tools, 'llvm-ar'))
    if (archiver) write(`${tools}/llvm-ar`, '#!/bin/sh\nexit 0\n', true)
    write(`${tools}/llvm-nm`, '#!/bin/sh\nexit 0\n', true)
    write('android/build-rust-cdylib.sh', builder)
    write('rust-toolchain.toml', 'channel = "fixture"\n')
    write('Cargo.toml', '[workspace]\n')
    write('scripts/release/native-build-identity.js', '')
    write('bin/uname', `#!/bin/sh\necho ${host}\n`, true)
    const rustc = shellPath(write('bin/rustc', '#!/bin/sh\nexit 0\n', true))
    const capture = path.join(fixture, 'cargo-env')
    write(
      'bin/rustup',
      `#!/usr/bin/env node
// Real rustup is a binary. A /bin/sh fixture strips hyphenated environment
// names on Linux before the test can observe what the builder supplied.
const fs = require('fs')
switch (process.argv[2]) {
  case 'which': console.log(${JSON.stringify(rustc)}); break
  case 'target': console.log(${JSON.stringify(target)}); break
  case 'run':
    fs.writeFileSync(${JSON.stringify(capture)}, Object.entries(process.env).map(([key, value]) => key + '=' + value).join('\\n'))
    process.exit(77)
}
`,
      true
    )
    const node = shellPath(
      write(
        'bin/identity-node',
        '#!/bin/sh\nprintf "UBM_BUILD_SOURCE_DIGEST=fixture\\nUBM_BUILD_BINDING_SCHEMA=fixture\\n"\n',
        true
      )
    )
    const options = {
      encoding: 'utf8',
      env: {
        ...fixturePathEnvironment(path.join(fixture, 'bin'), process.env),
        ANDROID_NDK_HOME: shellPath(path.join(fixture, 'ndk')),
        NODE_BINARY: node
      }
    }
    // Verify admission to the actual shell boundary, not merely the native
    // Node PATH string. A bypassed shim must report its resolved path and host
    // here instead of masquerading as a compiler/archiver regression below.
    const preflight = spawnFixtureShellSync(path.join(fixture, 'bin'), ['-c', 'command -v uname; uname -s'], options)
    expect({ error: preflight.error, status: preflight.status, stderr: preflight.stderr }).toEqual({
      error: undefined,
      status: 0,
      stderr: ''
    })
    const [resolvedUname, actualHost] = preflight.stdout.trim().split(/\r?\n/)
    expect(shellPath(resolvedUname)).toContain(`${path.basename(fixture)}/bin/uname`)
    expect(actualHost).toBe(host)
    const result = spawnFixtureShellSync(
      path.join(fixture, 'bin'),
      [
        shellPath(path.join(fixture, 'android/build-rust-cdylib.sh')),
        '--abi',
        abi,
        '--libdir',
        shellPath(path.join(fixture, 'output')),
        '--minsdk',
        '26'
      ],
      options
    )
    return { result, capture, compiler, ar }
  }

  test('Windows fixture PATH prepends one executable directory without a drive-letter delimiter or duplicate Path key', () => {
    const environment = fixturePathEnvironment(
      'D:\\runner temp\\bin',
      { Path: 'C:\\Program Files\\Git\\usr\\bin;C:\\node', KEEP: 'value' },
      ';'
    )
    expect(environment.PATH).toBe('D:\\runner temp\\bin;C:\\Program Files\\Git\\usr\\bin;C:\\node')
    expect(Object.keys(environment).filter(key => key.toLowerCase() === 'path')).toEqual(['PATH'])
    expect(environment.KEEP).toBe('value')
  })

  test('POSIX fixture PATH preserves existing command search after the executable mocks', () => {
    expect(fixturePathEnvironment('/tmp/fixture/bin', { PATH: '/usr/bin:/bin' }, ':').PATH).toBe(
      '/tmp/fixture/bin:/usr/bin:/bin'
    )
  })

  test('shell admission restores fixture priority after launcher-owned tools take precedence', () => {
    write('bin/uname', '#!/bin/sh\nprintf "fixture-host\\n"\n', true)
    // Model Git's wrapper prepending its own tools before the fixture entry.
    // A correct native PATH alone cannot assert the final POSIX search order.
    const inheritedPath = Object.entries(process.env).find(([key]) => key.toLowerCase() === 'path')[1]
    const bin = path.join(fixture, 'bin')
    const env = fixturePathEnvironment('', process.env)
    env.PATH = `${inheritedPath}${path.delimiter}${bin}`
    const opaqueArgument = 'space " quote $ dollar'
    const result = spawnFixtureShellSync(
      bin,
      ['-c', 'command -v uname; uname -s; command -v node; printf "%s\\n" "$1"', 'fixture-probe', opaqueArgument],
      { encoding: 'utf8', env }
    )
    expect({ error: result.error, status: result.status, stderr: result.stderr }).toEqual({
      error: undefined,
      status: 0,
      stderr: ''
    })
    const [resolved, host, inheritedNode, observedArgument] = result.stdout.trim().split(/\r?\n/)
    expect(shellPath(resolved)).toContain(`${path.basename(fixture)}/bin/uname`)
    expect(host).toBe('fixture-host')
    expect(inheritedNode).not.toBe('')
    expect(observedArgument).toBe(opaqueArgument)
  })

  test.each([
    ['Linux', 'armeabi-v7a', 'armv7-linux-androideabi'],
    ['Linux', 'arm64-v8a', 'aarch64-linux-android'],
    ['Linux', 'x86_64', 'x86_64-linux-android'],
    ['Darwin', 'arm64-v8a', 'aarch64-linux-android'],
    ['Darwin', 'armeabi-v7a', 'armv7-linux-androideabi'],
    ['Darwin', 'x86_64', 'x86_64-linux-android']
  ])('%s %s supplies target-specific compiler and archiver to Cargo', (host, abi, target) => {
    const { result, capture, compiler, ar } = run(host, abi, target)
    expect(result.stderr).toContain('cargo build failed')
    const environment = fs.readFileSync(capture, 'utf8').split('\n')
    expect(environment).toContain(`CC_${target}=${compiler}`)
    expect(environment).toContain(`AR_${target}=${ar}`)
    expect(environment).toContain(`CARGO_TARGET_${target.toUpperCase().replace(/-/g, '_')}_LINKER=${compiler}`)
  })

  test('missing archiver fails before Cargo with an actionable diagnostic', () => {
    const { result, capture } = run('Linux', 'arm64-v8a', 'aarch64-linux-android', false)
    expect(result.status).toBe(1)
    expect(result.stderr).toContain('NDK archiver missing:')
    expect(fs.existsSync(capture)).toBe(false)
  })
})
