// __tests__/AndroidConsumerKeepRules.test.js
const fs = require('node:fs')
const path = require('node:path')

const root = path.join(__dirname, '..')
const javaRoot = path.join(root, 'android/src/main/java')
const abis = ['armeabi-v7a', 'arm64-v8a', 'x86_64']
const jsiBinding = 'com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolJsiBinding'
const rules = fs.readFileSync(path.join(root, 'android/consumer-rules.pro'), 'utf8')
const ruleLines = rules.split('\n').filter(line => !line.trim().startsWith('#'))

const classGlobToRegExp = glob =>
  new RegExp(
    `^${glob
      .replace(/[.$]/g, '\\$&')
      .replace(/\*\*/g, '\u0000')
      .replace(/\*/g, '[^.]*')
      .replace(/\u0000/g, '.*')}$`
  )

const fullKeepPatterns = ruleLines
  .map(line => /^-keep class (\S+) \{ \*; \}$/.exec(line.trim()))
  .filter(Boolean)
  .map(match => classGlobToRegExp(match[1]))

const isFullyKept = binaryName =>
  fullKeepPatterns.some(pattern => pattern.test(binaryName.replace(/\//g, '.')))

const walk = directory =>
  fs.readdirSync(directory, { withFileTypes: true }).flatMap(entry => {
    const full = path.join(directory, entry.name)
    return entry.isDirectory() ? walk(full) : [full]
  })

// Binary names of every class declared under com/ubm and the protocol JSI binding
// (top-level files plus nested declarations as Outer$Inner).
const declaredClasses = new Set()
for (const file of walk(javaRoot).filter(f => /\.(java|kt)$/.test(f))) {
  const top = path.relative(javaRoot, file).replace(/\.(java|kt)$/, '')
  declaredClasses.add(top)
  for (const match of fs.readFileSync(file, 'utf8').matchAll(/\b(?:class|interface|enum)\s+([A-Za-z0-9_]+)/g)) {
    declaredClasses.add(`${top}$${match[1]}`)
  }
}
const classExists = binaryName => declaredClasses.has(binaryName)

describe('Android consumer R8/ProGuard keep rules', () => {
  test('rules ship in the AAR and the npm package, wired through Gradle', () => {
    const pkg = JSON.parse(fs.readFileSync(path.join(root, 'package.json'), 'utf8'))
    expect(pkg.files).toContain('android')
    expect(pkg.files).not.toContain('!android/consumer-rules.pro')
    const buildGradle = fs.readFileSync(path.join(root, 'android/build.gradle'), 'utf8')
    expect(buildGradle).toContain('consumerProguardFiles "consumer-rules.pro"')
    const verifier = fs.readFileSync(path.join(root, 'scripts/ci/verify-package-tarballs.js'), 'utf8')
    expect(verifier).toContain("'package/android/consumer-rules.pro'")
  })

  test('keeps every com.ubm class (the JNI bridges and their exception types)', () => {
    expect(isFullyKept('com/ubm/echo/EchoException')).toBe(true)
    expect(isFullyKept('com/ubm/core/MobileCoreBridge$MobileCoreException')).toBe(true)
    expect(isFullyKept('com/ubm/core/MobileCoreBridge$RadioHost')).toBe(true)
    const shipped = walk(path.join(javaRoot, 'com/ubm'))
    expect(shipped.length).toBeGreaterThan(0)
    for (const file of shipped) {
      expect(isFullyKept(path.relative(javaRoot, file).replace(/\.(java|kt)$/, ''))).toBe(true)
    }
  })

  test.each(abis)('every app class name embedded in the shipped %s library is covered', abi => {
    const binary = fs
      .readFileSync(path.join(root, 'android/src/main/jniLibs', abi, 'libubm5_jni_echo.so'))
      .toString('latin1')
    const referenced = new Set()
    for (const match of binary.matchAll(/com\/(?:ubm|sfourdrinier)\//g)) {
      // Adjacent rodata strings can be glued on; trim to the real class.
      let candidate = /^[A-Za-z0-9_/$]+/.exec(binary.slice(match.index, match.index + 200))[0]
      while (candidate.length > 0 && !classExists(candidate)) {
        candidate = candidate.slice(0, -1)
      }
      expect(candidate).not.toBe('')
      referenced.add(candidate)
    }
    for (const binaryName of referenced) {
      expect(isFullyKept(binaryName)).toBe(true)
    }
    expect(referenced).toContain('com/ubm/echo/EchoException')
    expect(referenced).toContain('com/ubm/core/MobileCoreBridge$MobileCoreException')
  })

  test('every Java_ native entry exported by the Rust sources maps to a kept com.ubm class', () => {
    const exported = new Set()
    for (const file of walk(path.join(root, 'bindings/jni/src')).filter(f => f.endsWith('.rs'))) {
      for (const match of fs.readFileSync(file, 'utf8').matchAll(/\bJava_com_ubm_([a-z]+)_([A-Za-z0-9]+)_[A-Za-z0-9]+/g)) {
        exported.add(`com/ubm/${match[1]}/${match[2]}`)
      }
    }
    expect(exported.size).toBeGreaterThan(0)
    for (const binaryName of exported) {
      expect(classExists(binaryName)).toBe(true)
      expect(isFullyKept(binaryName)).toBe(true)
    }
  })

  test('callback interfaces looked up by name stay named in their implementers', () => {
    const bridge = fs.readFileSync(path.join(javaRoot, 'com/ubm/core/MobileCoreBridge.java'), 'utf8')
    for (const iface of ['RadioHost', 'WakeListener', 'InvokeCallback']) {
      expect(bridge).toMatch(new RegExp(`public interface ${iface}\\b`))
      expect(rules).toContain(
        `-keepclassmembers class * implements com.ubm.core.MobileCoreBridge$${iface} { <methods>; }`
      )
    }
  })

  test('C++ FindClass/GetStaticMethodID targets are explicitly kept', () => {
    const classes = new Set()
    const methods = new Set()
    for (const file of walk(path.join(root, 'android/src/main/jni')).filter(f => /\.(cpp|hpp)$/.test(f))) {
      const text = fs.readFileSync(file, 'utf8')
      for (const match of text.matchAll(/FindClass\(\s*"(com\/[A-Za-z0-9_/$]+)"\s*\)/g)) classes.add(match[1])
      for (const match of text.matchAll(/GetStaticMethodID\(\s*binding\s*,\s*"([A-Za-z0-9_]+)"/g)) methods.add(match[1])
    }
    expect([...classes]).toEqual([jsiBinding])
    expect(methods.size).toBeGreaterThan(0)
    const block = new RegExp(`-keep class ${jsiBinding.replace(/\//g, '\\.')} \\{([^}]*)\\}`).exec(rules)
    expect(block).not.toBeNull()
    for (const method of methods) {
      expect(block[1]).toMatch(new RegExp(`static void ${method}\\(`))
    }
  })

  test('JNI native methods keep their names in the app-owned packages', () => {
    expect(rules).toMatch(
      /-keepclasseswithmembernames class com\.ubm\.\*\*, com\.sfourdrinier\.unifiedblemanager\.\*\* \{\s*native <methods>;\s*\}/
    )
  })
})
