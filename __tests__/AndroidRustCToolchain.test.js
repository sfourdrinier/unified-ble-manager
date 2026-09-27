const fs = require('fs')
const os = require('os')
const path = require('path')
const { spawnSync } = require('child_process')

const builder = fs.readFileSync(path.join(__dirname, '../android/build-rust-cdylib.sh'), 'utf8')

describe('Android canonical C dependency toolchain', () => {
  let fixture
  beforeEach(() => {
    fixture = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-android-c-'))
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
    const compiler = write(`${tools}/${target}26-clang`, '#!/bin/sh\nexit 0\n', true)
    const ar = path.join(fixture, tools, 'llvm-ar')
    if (archiver) write(`${tools}/llvm-ar`, '#!/bin/sh\nexit 0\n', true)
    write(`${tools}/llvm-nm`, '#!/bin/sh\nexit 0\n', true)
    write('android/build-rust-cdylib.sh', builder)
    write('rust-toolchain.toml', 'channel = "fixture"\n')
    write('Cargo.toml', '[workspace]\n')
    write('scripts/release/native-build-identity.js', '')
    write('bin/uname', `#!/bin/sh\necho ${host}\n`, true)
    const rustc = write('bin/rustc', '#!/bin/sh\nexit 0\n', true)
    const capture = path.join(fixture, 'cargo-env')
    write(
      'bin/rustup',
      `#!${process.execPath}
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
    const node = write(
      'bin/identity-node',
      '#!/bin/sh\nprintf "UBM_BUILD_SOURCE_DIGEST=fixture\\nUBM_BUILD_BINDING_SCHEMA=fixture\\n"\n',
      true
    )
    const result = spawnSync(
      'sh',
      [
        path.join(fixture, 'android/build-rust-cdylib.sh'),
        '--abi',
        abi,
        '--libdir',
        path.join(fixture, 'output'),
        '--minsdk',
        '26'
      ],
      {
        encoding: 'utf8',
        env: {
          ...process.env,
          PATH: `${path.join(fixture, 'bin')}:${process.env.PATH}`,
          ANDROID_NDK_HOME: path.join(fixture, 'ndk'),
          NODE_BINARY: node
        }
      }
    )
    return { result, capture, compiler, ar }
  }

  test.each([
    ['Linux', 'arm64-v8a', 'aarch64-linux-android'],
    ['Linux', 'x86_64', 'x86_64-linux-android'],
    ['Darwin', 'arm64-v8a', 'aarch64-linux-android'],
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
