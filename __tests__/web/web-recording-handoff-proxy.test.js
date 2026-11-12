const { webcrypto } = require('crypto')
const { createWebRecordingHandoffOwner, importWebRecordingHandoff } = require('../../src/web/web-recording-handoff')

const handoffOwner = options =>
  createWebRecordingHandoffOwner({
    ...options,
    graph: {},
    execute: (_graph, payload) => options.execute(payload)
  })

function bus(drop = () => false) {
  const members = new Set()
  const messages = []
  return {
    messages,
    channel() {
      const listeners = new Set()
      const channel = {
        postMessage(data) {
          messages.push(data)
          if (drop(data)) return
          for (const peer of members) if (peer !== channel) queueMicrotask(() => peer.deliver(data))
        },
        subscribe(fn) {
          listeners.add(fn)
          return () => listeners.delete(fn)
        },
        deliver(data) {
          for (const fn of listeners) fn(data)
        },
        close() {
          members.delete(channel)
        }
      }
      members.add(channel)
      return channel
    }
  }
}

const tick = () => new Promise(resolve => setTimeout(resolve, 15))

test('lost acceptance is retried and an abandoned acceptance expires', async () => {
  let dropped = false
  const network = bus(message => {
    if (!dropped && JSON.parse(message).t === 'accepted') {
      dropped = true
      return true
    }
    return false
  })
  const owner = await handoffOwner({
    crypto: webcrypto,
    channel: network.channel(),
    // The retry is asynchronous WebCrypto work on both peers. This test proves
    // recovery of a lost acceptance; the following case independently proves
    // short-lifetime abandonment, so do not make recovery depend on a narrow
    // scheduler window.
    lifetimeMs: 1500,
    execute: async x => x
  })
  const client = await importWebRecordingHandoff({
    crypto: webcrypto,
    channel: network.channel(),
    ticket: owner.ticket,
    timeoutMs: 1000
  })
  expect(await client.request('1', 'alive', 400)).toBe('alive')
  client.close()
  owner.close()

  const abandoned = bus(message => JSON.parse(message).t === 'accepted')
  const onClose = jest.fn()
  const other = await handoffOwner({
    crypto: webcrypto,
    channel: abandoned.channel(),
    lifetimeMs: 120,
    execute: async x => x,
    onClose
  })
  await expect(
    importWebRecordingHandoff({ crypto: webcrypto, channel: abandoned.channel(), ticket: other.ticket, timeoutMs: 60 })
  ).rejects.toMatchObject({ code: 'timeout' })
  await new Promise(resolve => setTimeout(resolve, 150))
  expect(onClose).toHaveBeenCalledTimes(1)
  other.close()
})

test('release waits until a lost operation receipt is recovered and retired', async () => {
  let dropped = false
  const network = bus(message => {
    if (!dropped && JSON.parse(message).d === 'owner') {
      dropped = true
      return true
    }
    return false
  })
  const execute = jest.fn(async () => 'effect')
  const onRelease = jest.fn()
  const owner = await handoffOwner({
    crypto: webcrypto,
    channel: network.channel(),
    lifetimeMs: 1000,
    execute,
    onRelease
  })
  const client = await importWebRecordingHandoff({
    crypto: webcrypto,
    channel: network.channel(),
    ticket: owner.ticket,
    timeoutMs: 500
  })
  await expect(client.request('1', 'work', 40)).rejects.toMatchObject({ code: 'timeout' })
  await expect(client.release(40)).rejects.toMatchObject({ code: 'timeout' })
  expect(onRelease).not.toHaveBeenCalled()
  expect(await client.request('1', 'work', 500)).toBe('effect')
  client.retire('1')
  await client.release(500)
  expect(onRelease).toHaveBeenCalledTimes(1)
  expect(execute).toHaveBeenCalledTimes(1)
  owner.close()
})

test('oversized result yields a replayable bounded completed outcome', async () => {
  let dropped = false
  const network = bus(message => {
    if (!dropped && JSON.parse(message).d === 'owner') {
      dropped = true
      return true
    }
    return false
  })
  const execute = jest.fn(async () => 'x'.repeat(65536))
  const owner = await handoffOwner({ crypto: webcrypto, channel: network.channel(), lifetimeMs: 1000, execute })
  const client = await importWebRecordingHandoff({
    crypto: webcrypto,
    channel: network.channel(),
    ticket: owner.ticket,
    timeoutMs: 500
  })
  await expect(client.request('1', 'work', 40)).rejects.toMatchObject({ code: 'timeout' })
  await expect(client.request('1', 'work', 500)).rejects.toMatchObject({
    code: 'result-too-large',
    outcome: 'completed'
  })
  expect(execute).toHaveBeenCalledTimes(1)
  client.close()
  owner.close()
})

test('unauthenticated floods are shed without closing the live owner', async () => {
  const network = bus()
  const onClose = jest.fn()
  const owner = await handoffOwner({
    crypto: webcrypto,
    channel: network.channel(),
    lifetimeMs: 1000,
    execute: async x => x,
    onClose
  })
  const client = await importWebRecordingHandoff({
    crypto: webcrypto,
    channel: network.channel(),
    ticket: owner.ticket,
    timeoutMs: 500
  })
  const attacker = network.channel()
  for (let i = 0; i < 100; i++)
    attacker.postMessage(
      JSON.stringify({ v: owner.ticket.version, r: owner.ticket.rendezvous, d: 'client', s: 1, c: '00', m: '00' })
    )
  await tick()
  expect(onClose).not.toHaveBeenCalled()
  expect(await client.request('1', 'still alive', 500)).toBe('still alive')
  client.close()
  owner.close()
  attacker.close()
})

test('more than 32 identical retries coalesce while execution is slow', async () => {
  const network = bus()
  let finish
  const execute = jest.fn(
    () =>
      new Promise(resolve => {
        finish = resolve
      })
  )
  const owner = await handoffOwner({ crypto: webcrypto, channel: network.channel(), lifetimeMs: 3000, execute })
  const client = await importWebRecordingHandoff({
    crypto: webcrypto,
    channel: network.channel(),
    ticket: owner.ticket,
    timeoutMs: 500
  })
  const first = client.request('1', 'work', 2000)
  while (!finish) await tick()
  const wire = network.messages.find(message => JSON.parse(message).d === 'client')
  const peer = network.channel()
  for (let i = 0; i < 100; i++) peer.postMessage(wire)
  await tick()
  finish('done')
  expect(await first).toBe('done')
  expect(execute).toHaveBeenCalledTimes(1)
  peer.close()
  client.close()
  owner.close()
})

test('normalized owner errors preserve typed safe platform details', async () => {
  const { BackendContractError } = require('../../src/backend-contract/errors')
  const normalized = {
    code: 'gatt.write-failed',
    domain: 'gatt',
    operation: 'write',
    retryability: 'never',
    commit: 'uncertain',
    platform: { domain: 'web', code: 'denied', safeMessage: 'refused', metadata: { reason: 'policy', number: 4 } }
  }
  const network = bus()
  const owner = await handoffOwner({
    crypto: webcrypto,
    channel: network.channel(),
    lifetimeMs: 1000,
    execute: async () => {
      throw new BackendContractError(normalized)
    }
  })
  const client = await importWebRecordingHandoff({
    crypto: webcrypto,
    channel: network.channel(),
    ticket: owner.ticket,
    timeoutMs: 500
  })
  await expect(client.request('1', 'work', 500)).rejects.toMatchObject({ normalized })
  client.close()
  owner.close()
})

test('lost confirmation and confirmation acknowledgement reconcile without a second endpoint', async () => {
  let confirmDropped = false
  let acknowledgementDropped = false
  const network = bus(message => {
    const type = JSON.parse(message).t
    if (type === 'confirm' && !confirmDropped) {
      confirmDropped = true
      return true
    }
    if (type === 'confirmed' && !acknowledgementDropped) {
      acknowledgementDropped = true
      return true
    }
    return false
  })
  const owner = await handoffOwner({
    crypto: webcrypto,
    channel: network.channel(),
    lifetimeMs: 250,
    execute: async x => x
  })
  const client = await importWebRecordingHandoff({
    crypto: webcrypto,
    channel: network.channel(),
    ticket: owner.ticket,
    timeoutMs: 200
  })
  await new Promise(resolve => setTimeout(resolve, 320))
  expect(confirmDropped && acknowledgementDropped).toBe(true)
  expect(await client.request('1', 'live', 300)).toBe('live')
  client.close()
  owner.close()
})

test('owner expires an acceptance when endpoint setup outlives the importer timeout', async () => {
  const network = bus()
  let resume
  const gate = new Promise(resolve => {
    resume = resolve
  })
  const subtle = new Proxy(webcrypto.subtle, {
    get(target, key) {
      if (key === 'deriveKey')
        return async (...args) => {
          await gate
          return target.deriveKey(...args)
        }
      const value = Reflect.get(target, key, target)
      return typeof value === 'function' ? value.bind(target) : value
    }
  })
  const ownerCrypto = { getRandomValues: webcrypto.getRandomValues.bind(webcrypto), subtle }
  const onClose = jest.fn()
  const owner = await handoffOwner({
    crypto: ownerCrypto,
    channel: network.channel(),
    lifetimeMs: 100,
    execute: async x => x,
    onClose
  })
  await expect(
    importWebRecordingHandoff({ crypto: webcrypto, channel: network.channel(), ticket: owner.ticket, timeoutMs: 50 })
  ).rejects.toMatchObject({ code: 'timeout' })
  await new Promise(resolve => setTimeout(resolve, 80))
  resume()
  await tick()
  expect(onClose).toHaveBeenCalledTimes(1)
  owner.close()
})

test('handshake sheds unauthenticated flood and still admits a valid importer', async () => {
  const network = bus()
  const owner = await handoffOwner({
    crypto: webcrypto,
    channel: network.channel(),
    lifetimeMs: 1000,
    execute: async x => x
  })
  const attacker = network.channel()
  for (let i = 0; i < 100; i++)
    attacker.postMessage(
      JSON.stringify({
        v: owner.ticket.version,
        r: owner.ticket.rendezvous,
        i: owner.ticket.incarnation,
        t: 'init',
        c: 'ab'.repeat(32),
        o: '',
        m: '00'
      })
    )
  const client = await importWebRecordingHandoff({
    crypto: webcrypto,
    channel: network.channel(),
    ticket: owner.ticket,
    timeoutMs: 500
  })
  expect(await client.request('1', 'live', 500)).toBe('live')
  attacker.close()
  client.close()
  owner.close()
})

test('endpoint setup finishing after the owner deadline cannot send acceptance', async () => {
  const network = bus()
  let resume
  let entered = false
  const gate = new Promise(resolve => {
    resume = resolve
  })
  const subtle = new Proxy(webcrypto.subtle, {
    get(target, key) {
      if (key === 'deriveKey')
        return async (...args) => {
          entered = true
          await gate
          return target.deriveKey(...args)
        }
      const value = Reflect.get(target, key, target)
      return typeof value === 'function' ? value.bind(target) : value
    }
  })
  let now = 0
  const onClose = jest.fn()
  const owner = await handoffOwner({
    crypto: { getRandomValues: webcrypto.getRandomValues.bind(webcrypto), subtle },
    channel: network.channel(),
    lifetimeMs: 1000,
    now: () => now,
    execute: async x => x,
    onClose
  })
  const importing = importWebRecordingHandoff({
    crypto: webcrypto,
    channel: network.channel(),
    ticket: owner.ticket,
    timeoutMs: 120
  })
  const failed = expect(importing).rejects.toMatchObject({ code: 'timeout' })
  while (!entered) await tick()
  now = 1001
  resume()
  await failed
  expect(network.messages.map(JSON.parse).some(packet => packet.t === 'accepted')).toBe(false)
  expect(onClose).toHaveBeenCalledTimes(1)
  owner.close()
})

test('oversized requests are refused before dispatch and leave the endpoint usable', async () => {
  const network = bus()
  const execute = jest.fn(async x => x)
  const owner = await handoffOwner({ crypto: webcrypto, channel: network.channel(), lifetimeMs: 1000, execute })
  const client = await importWebRecordingHandoff({
    crypto: webcrypto,
    channel: network.channel(),
    ticket: owner.ticket,
    timeoutMs: 500
  })
  await expect(client.request('1', 'x'.repeat(65536), 500)).rejects.toMatchObject({ code: 'size' })
  expect(execute).not.toHaveBeenCalled()
  expect(await client.request('1', 'small', 500)).toBe('small')
  client.close()
  owner.close()
})

test('oversized safe error detail yields a bounded failure receipt without exposing raw exceptions', async () => {
  const { BackendContractError } = require('../../src/backend-contract/errors')
  const network = bus()
  const execute = jest.fn(async () => {
    throw new BackendContractError({
      code: 'platform.failure',
      domain: 'platform',
      operation: 'read',
      retryability: 'never',
      platform: { domain: 'web', code: 'error', safeMessage: 'x'.repeat(10000), metadata: {} }
    })
  })
  const owner = await handoffOwner({ crypto: webcrypto, channel: network.channel(), lifetimeMs: 1000, execute })
  const client = await importWebRecordingHandoff({
    crypto: webcrypto,
    channel: network.channel(),
    ticket: owner.ticket,
    timeoutMs: 500
  })
  await expect(client.request('1', 'work', 500)).rejects.toMatchObject({
    code: 'error-detail-too-large',
    outcome: 'failed'
  })
  expect(execute).toHaveBeenCalledTimes(1)
  client.close()
  owner.close()
})

test('owner refuses a release that does not acknowledge its retained receipt', async () => {
  const { mac, WEB_PROXY_VERSION } = require('../../src/web/web-recording-proxy-crypto')
  const network = bus()
  const onRelease = jest.fn()
  const owner = await handoffOwner({
    crypto: webcrypto,
    channel: network.channel(),
    lifetimeMs: 1000,
    execute: async x => x,
    onRelease
  })
  const client = await importWebRecordingHandoff({
    crypto: webcrypto,
    channel: network.channel(),
    ticket: owner.ticket,
    timeoutMs: 500
  })
  expect(await client.request('1', 'work', 500)).toBe('work')
  const challenge = network.messages.map(JSON.parse).find(packet => packet.t === 'challenge')
  const signature = await mac(webcrypto, owner.ticket.secret, [
    WEB_PROXY_VERSION,
    owner.ticket.rendezvous,
    challenge.o,
    challenge.c,
    'control',
    'client',
    '0',
    'release:0'
  ])
  const attacker = network.channel()
  attacker.postMessage(
    JSON.stringify({
      v: WEB_PROXY_VERSION,
      r: owner.ticket.rendezvous,
      control: 'release:0',
      direction: 'client',
      signature
    })
  )
  await tick()
  expect(onRelease).not.toHaveBeenCalled()
  client.retire('1')
  await client.release(500)
  expect(onRelease).toHaveBeenCalledTimes(1)
  attacker.close()
  owner.close()
})

test('one-use handoff transports encrypted requests to the original owner and retains late results', async () => {
  const network = bus()
  let finish
  const execute = jest.fn(
    () =>
      new Promise(resolve => {
        finish = resolve
      })
  )
  const owner = await handoffOwner({
    crypto: webcrypto,
    channel: network.channel(),
    lifetimeMs: 10000,
    execute
  })
  const client = await importWebRecordingHandoff({
    crypto: webcrypto,
    channel: network.channel(),
    ticket: owner.ticket,
    timeoutMs: 1000
  })
  const pending = client.request('1', 'private payload', 10)
  await expect(pending).rejects.toMatchObject({ code: 'timeout' })
  while (!finish) await tick()
  expect(execute).toHaveBeenCalledTimes(1)
  finish('private result')
  await tick()
  expect(await client.request('1', 'private payload', 1000)).toBe('private result')
  expect(execute).toHaveBeenCalledTimes(1)
  const serialized = JSON.stringify(network.messages)
  expect(serialized).not.toContain('private payload')
  expect(serialized).not.toContain('private result')
  expect(serialized).not.toContain(owner.ticket.secret)
  await expect(
    importWebRecordingHandoff({ crypto: webcrypto, channel: network.channel(), ticket: owner.ticket, timeoutMs: 30 })
  ).rejects.toMatchObject({ code: 'timeout' })
  client.close()
  owner.close()
})

test('expiry and owner shutdown prevent acceptance; timeout does not authorize execution', async () => {
  const network = bus()
  let now = 1
  const execute = jest.fn()
  const owner = await handoffOwner({
    crypto: webcrypto,
    channel: network.channel(),
    lifetimeMs: 5,
    now: () => now,
    execute
  })
  now = 6
  await expect(
    importWebRecordingHandoff({ crypto: webcrypto, channel: network.channel(), ticket: owner.ticket, timeoutMs: 30 })
  ).rejects.toMatchObject({ code: 'timeout' })
  owner.close()
  expect(execute).not.toHaveBeenCalled()
})

test('concurrent retry has one identity and retirement requires the next sequence', async () => {
  const network = bus()
  let finish
  const execute = jest.fn(
    () =>
      new Promise(resolve => {
        finish = resolve
      })
  )
  const owner = await handoffOwner({
    crypto: webcrypto,
    channel: network.channel(),
    lifetimeMs: 10000,
    execute
  })
  const client = await importWebRecordingHandoff({
    crypto: webcrypto,
    channel: network.channel(),
    ticket: owner.ticket,
    timeoutMs: 1000
  })
  const a = client.request('1', 'same', 1000)
  const b = client.request('1', 'same', 1000)
  await expect(client.request('1', 'different', 1000)).rejects.toMatchObject({ code: 'pending' })
  while (!finish) await tick()
  finish('done')
  expect(await Promise.all([a, b])).toEqual(['done', 'done'])
  expect(execute).toHaveBeenCalledTimes(1)
  client.retire('1')
  await expect(client.request('1', 'same', 10)).rejects.toMatchObject({ code: 'identity' })
  client.close()
  owner.close()
})

test('tampered ciphertext and wrong ticket do not dispatch an operation', async () => {
  const network = bus()
  const execute = jest.fn(async value => value)
  const owner = await handoffOwner({
    crypto: webcrypto,
    channel: network.channel(),
    lifetimeMs: 10000,
    execute
  })
  const bad = { ...owner.ticket, secret: 'ab'.repeat(32) }
  await expect(
    importWebRecordingHandoff({ crypto: webcrypto, channel: network.channel(), ticket: bad, timeoutMs: 30 })
  ).rejects.toMatchObject({ code: 'timeout' })
  const client = await importWebRecordingHandoff({
    crypto: webcrypto,
    channel: network.channel(),
    ticket: owner.ticket,
    timeoutMs: 1000
  })
  expect(await client.request('1', 'first', 1000)).toBe('first')
  const captured = network.messages.map(m => JSON.parse(m)).find(m => m.d === 'client')
  captured.c = (captured.c[0] === '0' ? '1' : '0') + captured.c.slice(1)
  network.channel().postMessage(JSON.stringify(captured))
  await tick()
  expect(execute).toHaveBeenCalledTimes(1)
  client.retire('1')
  expect(await client.request('2', 'second', 1000)).toBe('second')
  expect(execute).toHaveBeenCalledTimes(2)
  client.close()
  owner.close()
})

test('explicit release informs the owner without canceling an admitted operation', async () => {
  const network = bus()
  let finish
  const ended = jest.fn()
  const owner = await handoffOwner({
    crypto: webcrypto,
    channel: network.channel(),
    lifetimeMs: 10000,
    onRelease: ended,
    execute: () =>
      new Promise(resolve => {
        finish = resolve
      })
  })
  const client = await importWebRecordingHandoff({
    crypto: webcrypto,
    channel: network.channel(),
    ticket: owner.ticket,
    timeoutMs: 1000
  })
  const result = client.request('1', 'work', 1000)
  while (!finish) await tick()
  const release = client.release()
  finish('completed')
  expect(await result).toBe('completed')
  client.retire('1')
  await release
  await tick()
  expect(ended).toHaveBeenCalledTimes(1)
  await expect(client.request('2', 'later', 10)).rejects.toMatchObject({ code: 'closed' })
  owner.close()
})

test('native-style channel adapter passes string envelopes and detaches on close', () => {
  const { createWebRecordingProxyChannel } = require('../../src/web/web-recording-proxy-channel')
  const listeners = new Set()
  const native = {
    postMessage: jest.fn(),
    addEventListener: (_type, fn) => listeners.add(fn),
    removeEventListener: (_type, fn) => listeners.delete(fn),
    start: jest.fn(),
    close: jest.fn()
  }
  const channel = createWebRecordingProxyChannel(native)
  const received = jest.fn()
  channel.subscribe(received)
  channel.postMessage('ciphertext')
  for (const listener of listeners) listener({ data: 'hello' })
  expect(received).toHaveBeenCalledWith('hello')
  expect(native.postMessage).toHaveBeenCalledWith('ciphertext')
  channel.close()
  expect(native.close).toHaveBeenCalledTimes(1)
  expect(listeners.size).toBe(0)
})

test('a valid MAC over different bytes at the same sequence closes the owner endpoint', async () => {
  const { mac, WEB_PROXY_VERSION } = require('../../src/web/web-recording-proxy-crypto')
  const network = bus()
  const execute = jest.fn(async value => value)
  const owner = await handoffOwner({
    crypto: webcrypto,
    channel: network.channel(),
    lifetimeMs: 10000,
    execute
  })
  const client = await importWebRecordingHandoff({
    crypto: webcrypto,
    channel: network.channel(),
    ticket: owner.ticket,
    timeoutMs: 1000
  })
  expect(await client.request('1', 'first', 1000)).toBe('first')
  const packets = network.messages.map(message => JSON.parse(message))
  const challenge = packets.find(packet => packet.t === 'challenge')
  const original = packets.find(packet => packet.d === 'client')
  const modified = { ...original, c: (original.c[0] === '0' ? '1' : '0') + original.c.slice(1) }
  modified.m = await mac(webcrypto, owner.ticket.secret, [
    WEB_PROXY_VERSION,
    owner.ticket.rendezvous,
    challenge.o,
    challenge.c,
    'client',
    '1',
    modified.c
  ])
  network.channel().postMessage(JSON.stringify(modified))
  await tick()
  client.retire('1')
  await expect(client.request('2', 'never executed', 50)).rejects.toMatchObject({ code: 'timeout' })
  expect(execute).toHaveBeenCalledTimes(1)
  client.close()
  owner.close()
})

test('a lost reply is replayed byte-for-byte without re-executing the request', async () => {
  let dropped = false
  const network = bus(message => {
    const packet = JSON.parse(message)
    if (!dropped && packet.d === 'owner') {
      dropped = true
      return true
    }
    return false
  })
  const execute = jest.fn(async () => 'answer')
  const owner = await handoffOwner({
    crypto: webcrypto,
    channel: network.channel(),
    lifetimeMs: 10000,
    execute
  })
  const client = await importWebRecordingHandoff({
    crypto: webcrypto,
    channel: network.channel(),
    ticket: owner.ticket,
    timeoutMs: 1000
  })
  await expect(client.request('1', 'payload', 50)).rejects.toMatchObject({ code: 'timeout' })
  expect(await client.request('1', 'payload', 1000)).toBe('answer')
  const replies = network.messages.filter(message => JSON.parse(message).d === 'owner')
  expect(replies).toHaveLength(2)
  expect(replies[0]).toBe(replies[1])
  expect(execute).toHaveBeenCalledTimes(1)
  client.close()
  owner.close()
})

test('expiry does not revoke an endpoint already accepted by the owner', async () => {
  const network = bus()
  let now = 0
  const owner = await handoffOwner({
    crypto: webcrypto,
    channel: network.channel(),
    lifetimeMs: 10000,
    now: () => now,
    execute: async value => value
  })
  const client = await importWebRecordingHandoff({
    crypto: webcrypto,
    channel: network.channel(),
    ticket: owner.ticket,
    timeoutMs: 1000
  })
  now = 10001
  expect(await client.request('1', 'still live', 1000)).toBe('still live')
  client.close()
  owner.close()
})

test('owner can explicitly revoke an accepted endpoint', async () => {
  const network = bus()
  const owner = await handoffOwner({
    crypto: webcrypto,
    channel: network.channel(),
    lifetimeMs: 10000,
    execute: async value => value
  })
  const client = await importWebRecordingHandoff({
    crypto: webcrypto,
    channel: network.channel(),
    ticket: owner.ticket,
    timeoutMs: 1000
  })
  await owner.revoke()
  await tick()
  await expect(client.request('1', 'no operation', 100)).rejects.toMatchObject({ code: 'closed' })
  client.close()
})

test('the owner dispatches every operation against the same original graph object', async () => {
  const network = bus()
  const graph = Object.freeze({})
  const graphs = []
  const owner = await createWebRecordingHandoffOwner({
    crypto: webcrypto,
    channel: network.channel(),
    lifetimeMs: 10000,
    graph,
    execute: async (actualGraph, payload) => {
      graphs.push(actualGraph)
      return payload
    }
  })
  const client = await importWebRecordingHandoff({
    crypto: webcrypto,
    channel: network.channel(),
    ticket: owner.ticket,
    timeoutMs: 1000
  })
  expect(await client.request('1', 'first', 1000)).toBe('first')
  client.retire('1')
  expect(await client.request('2', 'second', 1000)).toBe('second')
  expect(graphs).toEqual([graph, graph])
  expect(graphs[0]).toBe(graph)
  client.close()
  owner.close()
})

test('request timeout also bounds the wait for asynchronous encryption', async () => {
  const { createWebRecordingProxy } = require('../../src/web/web-recording-proxy')
  const network = bus()
  const crypto = {
    getRandomValues: webcrypto.getRandomValues.bind(webcrypto),
    subtle: {
      importKey: webcrypto.subtle.importKey.bind(webcrypto.subtle),
      deriveKey: webcrypto.subtle.deriveKey.bind(webcrypto.subtle),
      encrypt: () => new Promise(() => {})
    }
  }
  const client = await createWebRecordingProxy({
    crypto,
    channel: network.channel(),
    secret: 'aa'.repeat(32),
    rendezvous: 'bb'.repeat(32),
    ownerChallenge: 'cc'.repeat(32),
    clientChallenge: 'dd'.repeat(32),
    role: 'client'
  })
  await expect(client.request('1', 'work', 30)).rejects.toMatchObject({ code: 'timeout' })
  client.close()
})

test('release wait is bounded while encryption is stalled and keeps the endpoint fenced', async () => {
  const { createWebRecordingProxy } = require('../../src/web/web-recording-proxy')
  const network = bus()
  const crypto = {
    getRandomValues: webcrypto.getRandomValues.bind(webcrypto),
    subtle: {
      importKey: webcrypto.subtle.importKey.bind(webcrypto.subtle),
      deriveKey: webcrypto.subtle.deriveKey.bind(webcrypto.subtle),
      encrypt: () => new Promise(() => {})
    }
  }
  const client = await createWebRecordingProxy({
    crypto,
    channel: network.channel(),
    secret: 'aa'.repeat(32),
    rendezvous: 'bb'.repeat(32),
    ownerChallenge: 'cc'.repeat(32),
    clientChallenge: 'dd'.repeat(32),
    role: 'client'
  })
  await expect(client.request('1', 'work', 30)).rejects.toMatchObject({ code: 'timeout' })
  await expect(client.release(30)).rejects.toMatchObject({ code: 'timeout' })
  await expect(client.request('1', 'work', 30)).rejects.toMatchObject({ code: 'timeout' })
  await expect(client.request('2', 'other', 30)).rejects.toMatchObject({ code: 'closed' })
  client.close()
})

test('a different partition cannot rendezvous and abrupt owner loss leaves an unknown outcome', async () => {
  const source = bus()
  const otherPartition = bus()
  const execute = jest.fn(() => new Promise(() => {}))
  const owner = await handoffOwner({ crypto: webcrypto, channel: source.channel(), lifetimeMs: 10000, execute })
  await expect(
    importWebRecordingHandoff({
      crypto: webcrypto,
      channel: otherPartition.channel(),
      ticket: owner.ticket,
      timeoutMs: 30
    })
  ).rejects.toMatchObject({ code: 'timeout' })
  const client = await importWebRecordingHandoff({
    crypto: webcrypto,
    channel: source.channel(),
    ticket: owner.ticket,
    timeoutMs: 1000
  })
  const request = client.request('1', 'work', 80)
  while (execute.mock.calls.length === 0) await tick()
  owner.close()
  await expect(request).rejects.toMatchObject({ code: 'timeout' })
  await expect(client.request('2', 'takeover', 10)).rejects.toMatchObject({ code: 'pending' })
  expect(execute).toHaveBeenCalledTimes(1)
  client.close()
})
