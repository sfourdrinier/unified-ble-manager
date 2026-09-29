import { test } from 'node:test'
import assert from 'node:assert/strict'
import { createRequire } from 'node:module'
import { EventEmitter } from 'node:events'
const require = createRequire(import.meta.url)
const { installProcessBridge, installAuthenticatedAppHandler, PROCESS_CHANNEL } = require('../process-bridge.cjs')

function fixture(dispatch = async () => 'native-envelope') {
  let invoke
  const sender = new EventEmitter()
  sender.mainFrame = { processId: 7, routingId: 11, url: 'file:///app/index.html?backend=corebluetooth' }
  sender.isDestroyed = () => false
  const window = { webContents: sender }
  const calls = []
  const close = installProcessBridge({
    ipcMain: {
      handle: (channel, handler) => {
        assert.equal(channel, PROCESS_CHANNEL)
        invoke = handler
      },
      removeHandler: channel => calls.push(channel)
    },
    window,
    documentUrl: 'file:///app/index.html',
    dispatch: async request => {
      calls.push(request)
      return dispatch(request)
    }
  })
  const event = { sender, senderFrame: sender.mainFrame, processId: 7, frameId: 11 }
  return { invoke: (...args) => invoke(...args), event, sender, close, calls }
}

test('only the exact live app main frame can invoke allowlisted controls', async () => {
  const f = fixture()
  const request = { operation: 'status', args: {} }
  assert.equal(await f.invoke(f.event, request), 'native-envelope')
  for (const event of [
    { ...f.event, sender: {} },
    { ...f.event, frameId: 12 },
    { ...f.event, senderFrame: { ...f.event.senderFrame, url: 'https://evil.invalid' } }
  ]) {
    await assert.rejects(f.invoke(event, request), /unauthorized/)
  }
  await assert.rejects(f.invoke(f.event, { operation: 'configure-directory', args: { directory: '/tmp' } }), /command/)
  assert.equal(f.calls.length, 1)
  f.close()
  await assert.rejects(f.invoke(f.event, request), /closed/)
})

test('navigation retires an in-flight reply without acknowledging its prepared data', async () => {
  let settle
  const f = fixture(
    () =>
      new Promise(resolve => {
        settle = resolve
      })
  )
  const pending = f.invoke(f.event, { operation: 'prepare-claim', args: { maxItems: 2, maxBytes: 1024 } })
  f.sender.emit('did-start-navigation', { isMainFrame: true, isSameDocument: false })
  settle('prepared-native-envelope')
  await assert.rejects(pending, /retired/)
  assert.equal(f.calls.length, 1)
  assert.equal(f.calls[0].operation, 'prepare-claim')
  f.close()
})

test('same-document and subframe navigation preserve the authorized main-frame generation', async () => {
  let settle
  const f = fixture(
    () =>
      new Promise(resolve => {
        settle = resolve
      })
  )
  const pending = f.invoke(f.event, { operation: 'prepare-claim', args: { maxItems: 2, maxBytes: 1024 } })
  f.sender.emit('did-start-navigation', { isMainFrame: true, isSameDocument: true })
  f.sender.emit('did-start-navigation', { isMainFrame: false, isSameDocument: false })
  settle('retained-prepared-envelope')
  assert.equal(await pending, 'retained-prepared-envelope')
  const next = f.invoke(f.event, { operation: 'status', args: {} })
  settle('status-envelope')
  assert.equal(await next, 'status-envelope')
  f.close()
})

test('outstanding limit rejects excess work and recovers capacity after settlement', async () => {
  const resolvers = []
  const f = fixture(
    () =>
      new Promise(resolve => {
        resolvers.push(resolve)
      })
  )
  const request = { operation: 'status', args: {} }
  const pending = Array.from({ length: 8 }, () => f.invoke(f.event, request))
  await assert.rejects(f.invoke(f.event, request), /outstanding/)
  assert.equal(resolvers.length, 8)
  resolvers.forEach(resolve => resolve('ok'))
  await Promise.all(pending)
  const next = f.invoke(f.event, request)
  resolvers.at(-1)('ok')
  assert.equal(await next, 'ok')
  f.close()
})

test('oversized requests and replies fail closed without implicit ACK', async () => {
  const f = fixture(async () => 'x'.repeat(9 * 1024 * 1024))
  await assert.rejects(
    f.invoke(f.event, { operation: 'execute', args: { declaration: 'x'.repeat(600 * 1024) } }),
    /size/
  )
  assert.equal(f.calls.length, 0)
  await assert.rejects(f.invoke(f.event, { operation: 'prepare-claim', args: {} }), /size/)
  assert.equal(f.calls.length, 1)
  f.close()
})

test('lazy BLE app handler authenticates and validates before dispatch can acquire a radio', async () => {
  const f = fixture()
  let handler
  let acquired = 0
  const close = installAuthenticatedAppHandler({
    ipcMain: {
      handle: (channel, value) => {
        assert.equal(channel, 'ble')
        handler = value
      },
      removeHandler() {}
    },
    window: { webContents: f.sender },
    documentUrl: 'file:///app/index.html',
    channel: 'ble',
    validateRequest: request => {
      if (request.kind !== 'bootstrap') throw new Error('invalid bootstrap')
    },
    dispatch: async (request, event) => {
      assert.equal(event, f.event)
      acquired++
      return request
    }
  })
  await assert.rejects(handler({ ...f.event, frameId: 0 }, { kind: 'bootstrap' }), /unauthorized/)
  await assert.rejects(handler(f.event, { kind: 'invalid' }), /invalid bootstrap/)
  assert.equal(acquired, 0)
  assert.deepEqual(await handler(f.event, { kind: 'bootstrap' }), { kind: 'bootstrap' })
  assert.equal(acquired, 1)
  close()
  f.close()
})
