// Exercise the exact Gradle ELF verifier with synthetic good and bad objects.
// The child Gradle process has an empty PATH, so a shell/readelf dependency
// would fail even on developer hosts that happen to have those tools.
const fs = require('fs')
const os = require('os')
const path = require('path')
const { spawnSync } = require('child_process')

const root = path.resolve(__dirname, '../..')
const java = path.join(process.env.JAVA_HOME || '', 'bin', process.platform === 'win32' ? 'java.exe' : 'java')
if (!process.env.JAVA_HOME || !fs.existsSync(java)) {
  throw new Error('check-android-prebuilt-verifier requires a valid JAVA_HOME')
}

function elf({ align = 16384, machine = 183, segment = 1, count = 1, tableOffset = 64 } = {}) {
  const bytes = Buffer.alloc(256)
  bytes.set([0x7f, 69, 76, 70, 2, 1, 1], 0)
  bytes.writeUInt16LE(3, 16) // ET_DYN
  bytes.writeUInt16LE(machine, 18)
  bytes.writeBigUInt64LE(BigInt(tableOffset), 32)
  bytes.writeUInt16LE(64, 52)
  bytes.writeUInt16LE(56, 54)
  bytes.writeUInt16LE(count, 56)
  bytes.writeUInt32LE(segment, 64)
  bytes.writeBigUInt64LE(0n, 72) // p_offset
  bytes.writeBigUInt64LE(256n, 96) // p_filesz
  bytes.writeBigUInt64LE(256n, 104) // p_memsz
  bytes.writeBigUInt64LE(BigInt(align), 112) // p_align
  return bytes
}

const temp = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-elf-verifier-'))
try {
  for (const [name, bytes] of Object.entries({
    valid: elf(),
    misaligned: elf({ align: 4096 }),
    wrongMachine: elf({ machine: 62 }),
    noLoad: elf({ segment: 2 }),
    truncatedTable: elf({ tableOffset: 240 }),
    truncatedHeader: Buffer.alloc(10)
  })) {
    fs.writeFileSync(path.join(temp, `${name}.so`), bytes)
  }
  fs.writeFileSync(path.join(temp, 'settings.gradle'), "rootProject.name = 'ubm-elf-verifier-test'\n")
  const verifier = path.join(root, 'android/elf-16k-verifier.gradle').replace(/\\/gu, '/').replace(/'/gu, "\\'")
  fs.writeFileSync(path.join(temp, 'build.gradle'), `
apply from: '${verifier}'
tasks.register('verifyFixtures') {
  doLast {
    assert verifyUbmElf16k(file('valid.so'), 'arm64-v8a').contains('PASS')
    [misaligned: 'invalid alignment', wrongMachine: 'machine', noLoad: 'no PT_LOAD',
      truncatedTable: 'truncated ELF program-header table', truncatedHeader: 'truncated ELF header'].each { name, detail ->
      try {
        verifyUbmElf16k(file("\${name}.so"), 'arm64-v8a')
        throw new GradleException("malformed fixture \${name} passed")
      } catch (GradleException error) {
        if (!error.message.contains(detail)) throw error
      }
    }
  }
}
`)
  const wrapper = path.join(root, 'example/android/gradle/wrapper/gradle-wrapper.jar')
  const result = spawnSync(java, ['-cp', wrapper, 'org.gradle.wrapper.GradleWrapperMain', '-p', temp,
    'verifyFixtures', '--no-daemon', '--console=plain'], {
    cwd: root,
    env: { ...process.env, PATH: '' },
    encoding: 'utf8',
    timeout: 120000
  })
  if (result.error || result.status !== 0) {
    throw new Error(`Gradle ELF fixture check failed: ${result.error || result.status}\n${result.stdout}\n${result.stderr}`)
  }
  console.log('Android prebuilt ELF verifier: valid ELF accepted; five malformed fixtures rejected; Gradle ran with empty PATH')
} finally {
  fs.rmSync(temp, { recursive: true, force: true })
}
