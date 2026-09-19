const fs = require('fs')
const path = require('path')

const root = path.resolve(__dirname, '..')

describe('React Native native entropy', () => {
  test('the production TurboModule spec (UnifiedBleRustCore) declares randomBytes', () => {
    const spec = fs.readFileSync(path.join(root, 'src/NativeUnifiedBleRustCore.ts'), 'utf8')
    expect(spec).toMatch(/randomBytes\(length: number\): Promise<string>/)
  })

  test('the legacy control spec keeps getRandomBytes until Phase 4 deletes it (reference only)', () => {
    const spec = fs.readFileSync(path.join(root, 'src/NativeUnifiedBleProtocolControl.ts'), 'utf8')
    expect(spec).toMatch(/getRandomBytes\(length: number\): Promise<number\[\]>/)
  })

  test('Android control module uses SecureRandom and rejects out-of-range lengths', () => {
    const source = fs.readFileSync(
      path.join(root, 'android/src/main/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolControlModule.java'),
      'utf8'
    )
    expect(source).toMatch(/void getRandomBytes\(double length, Promise promise\)/)
    expect(source).toContain('java.security.SecureRandom')
    expect(source).not.toContain('Base64')
    expect(source).toContain('n <= 0 || n > 1024')
  })

  test('Apple control module uses SecRandomCopyBytes and rejects out-of-range lengths', () => {
    const source = fs.readFileSync(path.join(root, 'ios/UnifiedBleProtocolControl.mm'), 'utf8')
    expect(source).toContain('SecRandomCopyBytes')
    expect(source).toContain('getRandomBytes')
    expect(source).toContain('kSecRandomDefault')
    expect(source).toMatch(/n <= 0 \|\| n > 1024/)
  })

  test('RN factory uses native entropy unless randomBytes is injected', () => {
    const source = fs.readFileSync(path.join(root, 'src/react-native-app-manager.ts'), 'utf8')
    expect(source).toContain('createNativeRandomBytesSource')
    expect(source).toContain('normalized.randomBytes')
    expect(source).toContain('binding.randomBytes')
    expect(source).not.toContain('NativeUnifiedBleProtocolControl')
  })

  test('native entropy source slices a decoded CSPRNG pool', async () => {
    const { createNativeRandomBytesSource } = require('../src/react-native-entropy')
    const pool = Array.from({ length: 40 }, () => 0xcd)
    const getRandomBytes = jest.fn(async length => {
      expect(length).toBe(40)
      return pool
    })
    const randomBytes = await createNativeRandomBytesSource(getRandomBytes)
    expect(getRandomBytes).toHaveBeenCalledTimes(1)
    expect(randomBytes(16)).toEqual(new Uint8Array(16).fill(0xcd))
    expect(randomBytes(16)).toEqual(new Uint8Array(16).fill(0xcd))
    expect(randomBytes(8)).toEqual(new Uint8Array(8).fill(0xcd))
  })
})
