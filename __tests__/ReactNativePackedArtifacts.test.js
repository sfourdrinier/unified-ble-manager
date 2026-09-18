// __tests__/ReactNativePackedArtifacts.test.js

const fs = require('fs')
const path = require('path')

const root = path.join(__dirname, '..')

describe('packed React Native host artifacts', () => {
  test('declares and verifies the source tree required by React Native Codegen', () => {
    const packageJson = require(path.join(root, 'package.json'))
    const tarballVerifierPath = path.join(root, 'scripts', 'ci', 'verify-package-tarballs.js')
    const tarballVerifier = fs.readFileSync(tarballVerifierPath, 'utf8')

    expect(packageJson.codegenConfig.jsSrcsDir).toBe('src')
    expect(packageJson.files).toContain('src')
    expect(fs.existsSync(path.join(root, 'src', 'NativeUnifiedBleRustCore.ts'))).toBe(true)
    expect(tarballVerifier).toContain('expectedCodegenSourceEntries')
    expect(tarballVerifier).toContain('Packed React Native Codegen source set differs')
  })

  test('the packed React Native and Expo entry graphs reach UnifiedBleRustCore and never the legacy protocol control', () => {
    const moduleRoot = path.join(root, 'lib', 'module')
    const visited = new Set()
    const pending = ['react-native.js', 'expo.js']
    while (pending.length > 0) {
      const file = path.normalize(pending.pop())
      if (visited.has(file)) continue
      visited.add(file)
      const source = fs.readFileSync(path.join(moduleRoot, file), 'utf8')
      for (const match of source.matchAll(/(?:from|import|require\()\s*["'](\.[^"']+)["']/g)) {
        const target = path.join(path.dirname(file), match[1])
        pending.push(target.endsWith('.js') ? target : `${target}.js`)
      }
    }
    const graph = [...visited]
    expect(graph.some(file => file.includes('NativeUnifiedBleProtocolControl'))).toBe(false)
    expect(graph.some(file => file.includes('react-native-android-provider'))).toBe(false)
    expect(graph.some(file => file.includes('react-native-apple-provider'))).toBe(false)
    const binding = fs.readFileSync(
      path.join(moduleRoot, 'backends', 'reactnative', 'react-native-rust-core-binding.js'),
      'utf8'
    )
    expect(graph).toContain(path.join('backends', 'reactnative', 'react-native-rust-core-binding.js'))
    expect(binding).toContain("TurboModuleRegistry.get('UnifiedBleRustCore')")
  })
})
