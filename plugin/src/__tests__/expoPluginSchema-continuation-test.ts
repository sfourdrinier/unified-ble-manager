import { validateUnifiedBleExpoPluginOptions } from '../expoPluginSchema'

const HR_SERVICE = '0000180d-0000-1000-8000-00805f9b34fb'
const HR_MEASUREMENT = '00002a37-0000-1000-8000-00805f9b34fb'

describe('Expo plugin background.continuation schema (BGS4)', () => {
  it('defaults to absent (record-only at runtime) when not configured', () => {
    expect(validateUnifiedBleExpoPluginOptions({}).background).toBeUndefined()
    expect(validateUnifiedBleExpoPluginOptions({ background: { android: { mode: 'none' } } }).background).toEqual({
      android: { mode: 'none' }
    })
  })

  it('accepts a native standing order beside background.ios/android', () => {
    const validated = validateUnifiedBleExpoPluginOptions({
      background: {
        android: { mode: 'none' },
        continuation: {
          onAppearance: 'native',
          peerId: 'a0:9e:1a:e9:b9:3d',
          resubscribe: [{ serviceUuid: HR_SERVICE, characteristicUuid: HR_MEASUREMENT }]
        }
      }
    })
    expect(validated.background?.continuation).toMatchObject({
      onAppearance: 'native',
      peerId: 'A0:9E:1A:E9:B9:3D',
      resubscribe: [
        {
          serviceUuid: HR_SERVICE,
          characteristicUuid: HR_MEASUREMENT,
          serviceOccurrence: 1,
          characteristicOccurrence: 1
        }
      ]
    })
  })

  it('accepts deferred strategies with their rc.1 shapes, so no breaking change later', () => {
    expect(() =>
      validateUnifiedBleExpoPluginOptions({
        background: { continuation: { onAppearance: 'headless-task' } }
      })
    ).toThrow()
    const headless = validateUnifiedBleExpoPluginOptions({
      background: { continuation: { onAppearance: 'headless-task', headlessTaskName: 'BleWakeTask' } }
    })
    expect(headless.background?.continuation?.headlessTaskName).toBe('BleWakeTask')
    const fgs = validateUnifiedBleExpoPluginOptions({
      background: {
        continuation: {
          onAppearance: 'foreground-service',
          foregroundService: { notification: { channelId: 'ble', channelName: 'BLE', title: 'BLE' } }
        }
      }
    })
    expect(fgs.background?.continuation?.foregroundService?.notification.title).toBe('BLE')
  })

  it('refuses unknown strategies and unknown keys fail-closed', () => {
    expect(() =>
      validateUnifiedBleExpoPluginOptions({ background: { continuation: { onAppearance: 'auto-magic' } } })
    ).toThrow()
    expect(() =>
      validateUnifiedBleExpoPluginOptions({
        background: { continuation: { onAppearance: 'record-only', retryMs: 5 } }
      })
    ).toThrow()
  })
})
