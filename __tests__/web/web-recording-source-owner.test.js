const { WebNotificationIngress } = require('../../src/web/web-notification-ingress')
const { WebRecordingSourceOwner } = require('../../src/web/web-recording-source-owner')

function fixture() {
  const ingress = new WebNotificationIngress()
  const a = {}, b = {}
  const members = [
    { id: 'a', characteristic: a, lease: { enable: jest.fn(async () => {}), release: jest.fn(async () => {}) } },
    { id: 'b', characteristic: b, lease: { enable: jest.fn(async () => {}), release: jest.fn(async () => {}) } }
  ]
  const current = { value: true }
  const owner = new WebRecordingSourceOwner(ingress, members, 2, 8, () => current.value)
  return { ingress, a, b, members, current, owner }
}

test('the source retains its selected recording version and rejects unknown versions before reservation', () => {
  const f = fixture()
  const owner = new WebRecordingSourceOwner(f.ingress, f.members, 2, 8, () => true, 2)
  expect(owner.recordingVersion).toBe(2)
  expect(() => new WebRecordingSourceOwner(f.ingress, f.members, 2, 8, () => true, 3)).toThrow('protocol.incompatible')
  expect(f.members[0].lease.enable).not.toHaveBeenCalled()
})

test('installs all routes before enable and wakes synchronously for values, loss and terminal', () => {
  const f = fixture()
  const wake = jest.fn()
  f.owner.reserve()
  const { source, leases } = f.owner.install(wake)
  expect(leases.map(x => x.id)).toEqual(['a', 'b'])
  f.ingress.admit(f.b, new Uint8Array([2]))
  expect(wake).toHaveBeenCalledTimes(1)
  expect(source.peek().record).toMatchObject({ memberId: '1', sourceMembership: { member: '1', epoch: '2' }, delivery: 'unknown', provenance: 'read-or-notification' })
  f.ingress.admit(f.a, new Uint8Array([1]))
  f.ingress.admit(f.a, new Uint8Array([3]))
  expect(wake).toHaveBeenCalledTimes(4)
  expect(source.terminalReason).toBe('overflow')
})

test('retired registrations get fresh membership epochs and exact FIFO ownership', async () => {
  const f = fixture()
  f.owner.reserve()
  const first = f.owner.install(() => {}).source
  first.retire('closed')
  expect(first.ackExact('1')).toBe(true)
  await f.owner.release()
  const nextOwner = new WebRecordingSourceOwner(f.ingress, f.members, 2, 8, () => true)
  nextOwner.reserve()
  const second = nextOwner.install(() => {}).source
  f.ingress.admit(f.a, new Uint8Array([5]))
  expect(second.peek().record.sourceMembership).toEqual({ member: '0', epoch: '3' })
})

test('rejects invalid graphs and duplicate instance mappings without registering a source', () => {
  const f = fixture()
  f.current.value = false
  expect(() => f.owner.reserve()).toThrow()
  f.current.value = true
  const invalid = new WebRecordingSourceOwner(f.ingress, [f.members[0], { ...f.members[1], characteristic: f.a }], 2, 8, () => true)
  expect(() => invalid.reserve()).toThrow()
  const group = f.ingress.registerRetained(new Map([[f.a, 'x']]), 1, 8)
  f.ingress.admit(f.a, new Uint8Array([1]))
  expect(group.peek().id).toBe('1')
})

test('release refuses undrained source and installation is single-use', async () => {
  const f = fixture()
  f.owner.reserve()
  const { source } = f.owner.install(() => {})
  expect(() => f.owner.install(() => {})).toThrow()
  source.retire('connection-lost')
  await expect(f.owner.release()).rejects.toThrow()
  expect(source.ackExact('1')).toBe(true)
  await expect(f.owner.release()).resolves.toBeUndefined()
  expect(f.owner.isCurrent()).toBe(false)
})

test('a late enable remains owned and release retries failed compensation', async () => {
  const f = fixture()
  let resolveEnable
  f.members[0].lease.enable.mockReturnValue(new Promise(resolve => { resolveEnable = resolve }))
  f.members[0].lease.release.mockRejectedValueOnce(new Error('cleanup failed'))
  f.owner.reserve()
  const { source, leases } = f.owner.install(() => {})
  const enabling = leases[0].enable()
  await Promise.resolve()
  source.retire('closed')
  const releasing = leases[0].release()
  await Promise.resolve()
  expect(f.members[0].lease.release).not.toHaveBeenCalled()
  resolveEnable()
  await enabling
  await expect(releasing).rejects.toThrow('cleanup failed')
  await expect(f.owner.release()).rejects.toThrow()
  await leases[0].release()
  source.ackExact('1')
  await f.owner.release()
})

test('a source fenced before dispatched enable never invokes its lease port', async () => {
  const f = fixture()
  f.owner.reserve()
  const { source, leases } = f.owner.install(() => {})
  const enabling = leases[0].enable()
  source.retire('connection-lost')
  await expect(enabling).rejects.toThrow()
  expect(f.members[0].lease.enable).not.toHaveBeenCalled()
  await leases[0].release()
})

test.each([
  ['connection-lost', 'connection.lost'],
  ['owner-released', 'operation.disconnected'],
  ['source-failed', 'operation.reset'],
  ['host-reset', 'backend.reset'],
  ['service-changed', 'gatt.stale-handle']
])('a %s event before enable dispatch retains its library origin', async (reason, code) => {
  const { trustedWebRecordingError } = require('../../src/web/web-recording-errors')
  const f = fixture()
  f.owner.reserve()
  const { source, leases } = f.owner.install(() => {})
  const enabling = leases[0].enable()
  source.retire(reason)
  const failure = await enabling.catch(error => error)
  expect(trustedWebRecordingError(failure)).toMatchObject({ normalized: { code } })
  expect(f.members[0].lease.enable).not.toHaveBeenCalled()
  await leases[0].release()
})

test('graph-bound construction validates paths and mode before installing anything', () => {
  const { createGraphBoundWebRecordingSourceOwner } = require('../../src/web/web-recording-source-owner')
  const f = fixture()
  f.a.properties = { notify: true, indicate: true }
  const path = { serviceUuid: 'service', connectionId: 'c', databaseId: 'd', databaseGeneration: '1', serviceOccurrence: '0', characteristicUuid: 'characteristic', characteristicOccurrence: '0' }
  const { characteristicKey } = require('../../src/web/web-bluetooth-handles')
  const db = { assertPath: jest.fn(), record: { valid: true, grantedServices: new Set(['service']), device: { gatt: { connected: true } } }, characteristicBoundaries: new Map([[characteristicKey(path), f.a]]) }
  db.record.database = db
  const member = { id: 'a', path, deliveryMode: 'require-indication', lease: f.members[0].lease }
  for (const deliveryMode of ['require-indication', 'require-notification']) {
    expect(() => createGraphBoundWebRecordingSourceOwner(db, f.ingress, [{ ...member, deliveryMode }], 2, 8, () => true))
      .toThrow('capability.limited')
  }
  expect(f.members[0].lease.enable).not.toHaveBeenCalled()
  const owner = createGraphBoundWebRecordingSourceOwner(db, f.ingress, [{ ...member, deliveryMode: 'prefer-notification' }], 2, 8, () => true)
  expect(db.assertPath).toHaveBeenCalledWith(path, expect.any(String))
  owner.reserve()
  owner.install(() => {})
  db.record.device.gatt.connected = false
  expect(owner.isCurrent()).toBe(false)
})

test('exposes a copy of the exact installed membership epochs to integration', () => {
  const f = fixture()
  expect(f.owner.memberships()).toBeNull()
  f.owner.reserve()
  f.owner.install(() => {})
  const memberships = f.owner.memberships()
  expect(memberships).toEqual([{ member: '0', epoch: '1' }, { member: '1', epoch: '2' }])
  memberships[0].epoch = '99'
  expect(f.owner.memberships()[0].epoch).toBe('1')
})

test('forwards the exact source acknowledgement and its asynchronous evidence barrier', async () => {
  const f = fixture()
  const delivery = { id: '7', record: { kind: 'marker', markerId: '2', ordinal: '9' } }
  let resolve
  const evidence = new Promise(done => { resolve = done })
  const acknowledged = jest.fn(() => evidence)
  f.owner.onAcknowledged = acknowledged
  const pending = f.owner.acknowledged(delivery)
  expect(acknowledged).toHaveBeenCalledWith(delivery)
  expect(pending).toBe(evidence)
  resolve()
  await pending
})

test('detaches a terminal source with retained deliveries only after all dispatched leases are released', async () => {
  const f = fixture()
  f.owner.reserve()
  const { source, leases } = f.owner.install(() => {})
  await leases[0].enable()
  f.ingress.admit(f.a, new Uint8Array([42]))
  source.retire('source-failed')
  await expect(f.owner.releaseRetainingDeliveries()).rejects.toThrow()
  await leases[0].release()
  await f.owner.releaseRetainingDeliveries()
  expect(f.owner.isInstalled()).toBe(false)
  expect(source.peek()).toMatchObject({ id: '1', record: { value: new Uint8Array([42]) } })
  expect(source.ackExact('1')).toBe(true)
  expect(source.peek().record.kind).toBe('terminal')
})

test('preallocates unique immutable epochs without installing routes and consumes each reservation once', () => {
  const changes = jest.fn()
  const ingress = new WebNotificationIngress(changes)
  const characteristic = {}
  const member = { id: 'a', characteristic, lease: { enable: jest.fn(), release: jest.fn() } }
  const first = new WebRecordingSourceOwner(ingress, [member], 2, 8, () => true)
  const second = new WebRecordingSourceOwner(ingress, [member], 2, 8, () => true)
  const reserved = first.reserve()
  expect(second.reserve().memberships()).toEqual([{ member: '0', epoch: '2' }])
  expect(reserved.memberships()).toEqual([{ member: '0', epoch: '1' }])
  reserved.memberships()[0].epoch = '999'
  expect(changes).not.toHaveBeenCalled()
  expect(first.install(() => {}).source.memberships()).toEqual([{ member: '0', epoch: '1' }])
  expect(() => first.install(() => {})).toThrow()
})

test('failed installation rolls back routes without reusing reserved epochs', () => {
  const changes = []
  const a = {}, b = {}
  const ingress = new WebNotificationIngress((target, added) => {
    changes.push([target, added])
    if (target === b && added) throw new Error('registration failed')
  })
  const member = (id, characteristic) => ({ id, characteristic, lease: { enable: jest.fn(), release: jest.fn() } })
  const first = new WebRecordingSourceOwner(ingress, [member('a', a), member('b', b)], 2, 8, () => true)
  first.reserve()
  expect(() => first.install(() => {})).toThrow('registration failed')
  expect(changes).toEqual([[a, true], [b, true], [b, false], [a, false]])
  const next = new WebRecordingSourceOwner(ingress, [member('a', a)], 2, 8, () => true)
  expect(next.reserve().memberships()).toEqual([{ member: '0', epoch: '3' }])
})

test('membership exhaustion refuses the whole reservation without installing routes', () => {
  const f = fixture()
  f.ingress.membershipEpoch = 18446744073709551614n
  expect(() => f.owner.reserve()).toThrow()
  expect(f.owner.memberships()).toBeNull()
  const next = new WebRecordingSourceOwner(f.ingress, [f.members[0]], 2, 8, () => true)
  expect(next.reserve().memberships()).toEqual([{ member: '0', epoch: '18446744073709551615' }])
})

test('failed route rollback keeps source ownership when cleanup is unproven', async () => {
  const characteristic = {}
  const ingress = new WebNotificationIngress((_target, added) => {
    throw new Error(added ? 'registration failed' : 'rollback failed')
  })
  const owner = new WebRecordingSourceOwner(ingress, [{ id: 'a', characteristic, lease: { enable: jest.fn(), release: jest.fn() } }], 2, 8, () => true)
  owner.reserve()
  expect(() => owner.install(() => {})).toThrow('registration cleanup failed')
  await expect(owner.release()).rejects.toThrow('cleanup')
})
