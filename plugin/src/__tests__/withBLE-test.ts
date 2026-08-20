import { applyNativeProtocolRestorationInfoPlist, validateBlePluginOptions } from '../withBLE'
import { isUnifiedBlePluginDebugEnabled } from '../debugLog'

const completeRestoration = {
  identifier: 'com.example.app.ble',
  namespace: 'com.example.app.ble',
  epoch: '2026-07-30',
  clientId: 'com.example.app.ble.client',
  hostSessionScope: 'com.example.app.ble.host'
}

describe('validateBlePluginOptions', () => {
  it('accepts only the complete, current plugin option shape', () => {
    expect(
      validateBlePluginOptions({
        debug: true,
        requiresBluetoothLeHardware: false,
        neverForLocation: true,
        modes: ['central'],
        bluetoothAlwaysPermission: false,
        iosNativeProtocolRestoration: completeRestoration
      })
    ).toEqual({
      debug: true,
      requiresBluetoothLeHardware: false,
      neverForLocation: true,
      modes: ['central'],
      bluetoothAlwaysPermission: false,
      iosNativeProtocolRestoration: completeRestoration
    })
  })

  it('rejects the retired isBackgroundEnabled option name', () => {
    expect(() => validateBlePluginOptions({ isBackgroundEnabled: true })).toThrow(
      /unsupported properties: isBackgroundEnabled/
    )
  })

  it('rejects iOS peripheral background mode', () => {
    expect(() => validateBlePluginOptions({ modes: ['peripheral'] })).toThrow(/peripheral/)
    expect(() => validateBlePluginOptions({ modes: ['central', 'peripheral'] })).toThrow(/peripheral/)
  })

  it.each([
    ['a non-object options value', null],
    ['an unknown option', { unexpected: true }],
    ['a string debug option', { debug: 'true' }],
    ['a string hardware-required option', { requiresBluetoothLeHardware: 'true' }],
    ['a numeric never-for-location option', { neverForLocation: 1 }],
    ['a scalar background mode', { modes: 'central' }],
    ['an unsupported background mode', { modes: ['observer'] }],
    ['a duplicate background mode', { modes: ['central', 'central'] }],
    ['an empty Bluetooth permission string', { bluetoothAlwaysPermission: '  ' }],
    ['a truthy Bluetooth permission boolean', { bluetoothAlwaysPermission: true }],
    ['a retired restoration option', { iosNativeProtocolRestorationIdentifier: 'com.example.app.ble' }],
    ['a partial restoration object', { iosNativeProtocolRestoration: { identifier: 'com.example.app.ble' } }],
    [
      'a restoration object with an unknown property',
      { iosNativeProtocolRestoration: { ...completeRestoration, unexpected: 'value' } }
    ],
    [
      'a restoration object with an empty required value',
      { iosNativeProtocolRestoration: { ...completeRestoration, clientId: '' } }
    ]
  ])('rejects %s', (_label, options) => {
    expect(() => validateBlePluginOptions(options)).toThrow()
  })
})

describe('applyNativeProtocolRestorationInfoPlist', () => {
  it('replaces every restoration value together', () => {
    const infoPlist: Record<string, unknown> = {
      UnifiedBleProtocolRestoreIdentifier: 'stale-identifier',
      UnifiedBleProtocolRestorationNamespace: 'stale-namespace',
      UnifiedBleProtocolRestorationEpoch: 'stale-epoch',
      UnifiedBleProtocolRestorationClientId: 'stale-client',
      UnifiedBleProtocolRestorationHostSessionScope: 'stale-scope'
    }

    applyNativeProtocolRestorationInfoPlist(infoPlist, completeRestoration)

    expect(infoPlist).toMatchObject({
      UnifiedBleProtocolRestoreIdentifier: completeRestoration.identifier,
      UnifiedBleProtocolRestorationNamespace: completeRestoration.namespace,
      UnifiedBleProtocolRestorationEpoch: completeRestoration.epoch,
      UnifiedBleProtocolRestorationClientId: completeRestoration.clientId,
      UnifiedBleProtocolRestorationHostSessionScope: completeRestoration.hostSessionScope
    })
  })

  it('removes the entire native restoration configuration when it is not configured', () => {
    const infoPlist: Record<string, unknown> = {
      UnifiedBleProtocolRestoreIdentifier: 'stale-identifier',
      UnifiedBleProtocolRestorationNamespace: 'stale-namespace',
      UnifiedBleProtocolRestorationEpoch: 'stale-epoch',
      UnifiedBleProtocolRestorationClientId: 'stale-client',
      UnifiedBleProtocolRestorationHostSessionScope: 'stale-scope'
    }

    applyNativeProtocolRestorationInfoPlist(infoPlist)

    expect(infoPlist).toEqual({})
  })
})

describe('plugin debug environment', () => {
  const previousUnified = process.env.UNIFIED_BLE_MANAGER_PLUGIN_DEBUG
  const previousLegacy = process.env.BLEPLX_PLUGIN_DEBUG

  afterEach(() => {
    if (previousUnified === undefined) {
      delete process.env.UNIFIED_BLE_MANAGER_PLUGIN_DEBUG
    } else {
      process.env.UNIFIED_BLE_MANAGER_PLUGIN_DEBUG = previousUnified
    }
    if (previousLegacy === undefined) {
      delete process.env.BLEPLX_PLUGIN_DEBUG
    } else {
      process.env.BLEPLX_PLUGIN_DEBUG = previousLegacy
    }
  })

  it('enables debug from UNIFIED_BLE_MANAGER_PLUGIN_DEBUG', () => {
    delete process.env.BLEPLX_PLUGIN_DEBUG
    process.env.UNIFIED_BLE_MANAGER_PLUGIN_DEBUG = '1'
    expect(isUnifiedBlePluginDebugEnabled()).toBe(true)
  })

  it('still enables debug from BLEPLX_PLUGIN_DEBUG', () => {
    delete process.env.UNIFIED_BLE_MANAGER_PLUGIN_DEBUG
    process.env.BLEPLX_PLUGIN_DEBUG = 'true'
    expect(isUnifiedBlePluginDebugEnabled()).toBe(true)
  })
})
