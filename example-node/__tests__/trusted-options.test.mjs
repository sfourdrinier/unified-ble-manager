import { test } from 'node:test'
import assert from 'node:assert/strict'
import { trustedDesktopOptions } from '../trusted-options.cjs'

test('trusted BlueZ owner and exact adapter reach one process-owner configuration', () => {
  assert.deepEqual(trustedDesktopOptions('bluez', '/org/bluez/hci1', ':1.42'), {
    adapterId: '/org/bluez/hci1',
    connectionPolicy: { mode: 'le-bearer', daemonUniqueOwner: ':1.42' }
  })
  assert.deepEqual(trustedDesktopOptions('bluez', undefined, undefined), {})
  assert.deepEqual(trustedDesktopOptions('corebluetooth', 'adapter', undefined), { adapterId: 'adapter' })
})

test('owner configuration refuses non-BlueZ hosts and never manufactures attestation', () => {
  for (const backend of ['corebluetooth', 'winrt'])
    assert.throws(() => trustedDesktopOptions(backend, undefined, ':1.42'), /only.*bluez/)
  assert.throws(() => trustedDesktopOptions('bluez', ' ', undefined), /adapter/)
  assert.deepEqual(
    trustedDesktopOptions('bluez', undefined, 'invalid'),
    {
      connectionPolicy: { mode: 'le-bearer', daemonUniqueOwner: 'invalid' }
    },
    'the public factory, not a second reference grammar, validates the owner'
  )
})
