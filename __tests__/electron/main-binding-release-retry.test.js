// __tests__/electron/main-binding-release-retry.test.js
//
// Finding F3: a renderer whose release persistently fails must not reschedule
// its 100 ms release retry forever, and binding teardown must disarm the
// retry timer so no callback fires after the router is destroyed. After the
// bound is reached the binding reports the terminal release-failed record
// instead of retrying silently.

'use strict'

const { ElectronMainBleBinding, ElectronMainBleRouter } = require('../../src/electron-main')
const { inspectElectronMainBleBindingForTests } = require('../../src/testing')
const { IPC_CLIENT_COMPATIBILITY_OFFER } = require('../../src/ipc/protocol')
const { monotonicTimestamp, opaqueId, version, versionRange } = require('../../src/backend-contract/primitives')

const RELEASE_FAILURE_MESSAGE = '[ElectronMainBleBinding] Renderer lifetime cleanup reported failures:'
const EXHAUSTION_MESSAGE = '[ElectronMainBleBinding] Renderer release retries exhausted; reporting release-failed:'

function negotiated(axis) {
  const selected = version(axis, axis === 'ipc-protocol' ? 4 : 1)
  const range = versionRange(selected, selected)
  return { axis, selected, localRange: range, remoteRange: range }
}

function attachment() {
  const backendGeneration = opaqueId('electron-generation', 'backend-generation', 'electron')
  return {
    attachmentId: opaqueId('electron-attachment', 'attachment', 'electron'),
    backendInstanceId: opaqueId('electron-backend', 'backend-instance', 'electron'),
    backendGeneration,
    adapter: {
      adapterId: opaqueId('electron-adapter', 'adapter', 'electron'),
      displayName: null,
      state: {
        availability: 'available',
        authorization: 'granted',
        power: 'on',
        heard: null,
        backendGeneration,
        updatedAt: monotonicTimestamp(1),
        safeReason: null
      },
      adapterGeneration: opaqueId('electron-adapter-generation', 'adapter-generation', 'electron'),
      limitations: []
    }
  }
}

function versions() {
  return {
    backendContract: negotiated('backend-contract'),
    capabilitySchema: negotiated('capability-schema'),
    eventSchema: negotiated('event-schema'),
    traceFormat: negotiated('trace-format')
  }
}

function createSender(client, windowScope, sessionScope) {
  const destroyedListeners = []
  const mainFrame = Object.freeze({ processId: 10, routingId: 20 })
  let destroyed = false
  return {
    mainFrame,
    trusted: {
      authenticatedClientId: opaqueId(client, 'client', `electron:${client}`),
      authenticatedWindowScope: windowScope,
      authenticatedSessionScope: sessionScope
    },
    isDestroyed: () => destroyed,
    once: (event, listener) => {
      if (event === 'destroyed') {
        destroyedListeners.push(listener)
      }
    },
    on: () => undefined,
    removeListener: () => undefined,
    send() {},
    destroy() {
      destroyed = true
      const listeners = destroyedListeners.splice(0, destroyedListeners.length)
      for (const listener of [...listeners]) {
        listener()
      }
    }
  }
}

function released() {
  return { state: 'released', failures: [] }
}

function failed(resourceKind) {
  return {
    state: 'release-failed',
    failures: [
      {
        resourceKind,
        error: {
          code: 'platform.failure',
          domain: 'cleanup',
          operation: `test.${resourceKind}`,
          platform: null,
          retryability: 'transient'
        }
      }
    ]
  }
}

function createMainFixture(managerOverrides = {}) {
  const currentAttachment = attachment()
  const manager = {
    attachedBackend: { attachment: { attachment: currentAttachment } },
    identity: { versions: versions() },
    capabilities: () => [],
    planScan: jest.fn(),
    onAttachmentAdvanced: () => () => undefined,
    destroy: jest.fn(async () => ({ state: 'released', failures: [] })),
    ...managerOverrides
  }
  const router = new ElectronMainBleRouter({
    manager,
    maximumMessageBytes: 4096,
    maximumOutstandingOperations: 2,
    maximumRetainedBytes: 8192,
    publish: async () => 'terminalized'
  })
  const port = {
    handler: null,
    handle(channel, handler) {
      expect(channel).toBe('unified-ble-manager:v2')
      this.handler = (event, request) =>
        handler(
          {
            ...event,
            frameId: event.frameId ?? event.sender.mainFrame.routingId,
            processId: event.processId ?? event.sender.mainFrame.processId
          },
          request
        )
    },
    removeHandler: jest.fn()
  }
  const authenticate = jest.fn(event => event.sender.trusted)
  const binding = new ElectronMainBleBinding({ router, port, authenticate })
  binding.install()
  return { authenticate, binding, currentAttachment, manager, port, router, versions: manager.identity.versions }
}

async function bootstrap(current, sender) {
  const response = await current.port.handler({ sender }, { kind: 'bootstrap', offer: IPC_CLIENT_COMPATIBILITY_OFFER })
  expect(response.kind).toBe('bootstrap')
  return response.bootstrap
}

async function flushAsyncWork() {
  for (let index = 0; index < 6; index += 1) {
    await Promise.resolve()
  }
}

// Consume queued console.error diagnostics for one message shape; each guard
// expectation consumes exactly one entry, so drain in a loop and count.
function drainConsoleErrors(message, leaseId) {
  let drained = 0
  for (;;) {
    try {
      expectConsoleErrorMatching(message, { rendererLeaseId: leaseId, cleanup: expect.anything() })
      drained += 1
    } catch {
      break
    }
  }
  return drained
}

describe('main binding release retry bound (finding F3)', () => {
  test('a persistently failing renderer release stops retrying after the bound', async () => {
    jest.useFakeTimers()
    try {
      const current = createMainFixture()
      const sender = createSender('client-retry-bound', 'window-retry-bound', 'session-retry-bound')
      const renderer = await bootstrap(current, sender)
      const leaseId = String(renderer.rendererLease.leaseId)
      const releaseRenderer = jest
        .spyOn(current.router, 'releaseRenderer')
        .mockResolvedValue(failed('renderer-release'))

      sender.destroy()
      await flushAsyncWork()
      expect(releaseRenderer).toHaveBeenCalledTimes(1)
      expect(jest.getTimerCount()).toBe(1)
      expect(drainConsoleErrors(RELEASE_FAILURE_MESSAGE, leaseId)).toBe(1)

      let exhaustionReports = 0
      for (let tick = 0; tick < 40; tick += 1) {
        jest.advanceTimersByTime(100)
        await flushAsyncWork()
        drainConsoleErrors(RELEASE_FAILURE_MESSAGE, leaseId)
        exhaustionReports += drainConsoleErrors(EXHAUSTION_MESSAGE, leaseId)
      }

      expect(releaseRenderer.mock.calls.length).toBeLessThanOrEqual(31)
      expect(jest.getTimerCount()).toBe(0)
      expect(exhaustionReports).toBe(1)
      const inspection = inspectElectronMainBleBindingForTests(current.binding)
      const inspected = inspection.renderers.find(entry => entry.leaseId === leaseId)
      expect(inspected.releaseRetries).toBe(30)
      expect(inspected.retryExhausted).toBe(true)
      expect(inspected.lastReleaseFailure).toMatchObject({ state: 'release-failed' })
      await current.binding.destroy()
      drainConsoleErrors(RELEASE_FAILURE_MESSAGE, leaseId)
    } finally {
      jest.useRealTimers()
    }
  })

  test('binding teardown disarms the retry so no radio call fires after router destroy', async () => {
    jest.useFakeTimers()
    try {
      const current = createMainFixture()
      const sender = createSender('client-retry-teardown', 'window-retry-teardown', 'session-retry-teardown')
      const renderer = await bootstrap(current, sender)
      const leaseId = String(renderer.rendererLease.leaseId)
      const releaseRenderer = jest
        .spyOn(current.router, 'releaseRenderer')
        .mockResolvedValue(failed('renderer-release'))
      const routerDestroy = jest.spyOn(current.router, 'destroy').mockResolvedValue(failed('electron-router'))

      sender.destroy()
      await flushAsyncWork()
      expect(releaseRenderer).toHaveBeenCalledTimes(1)
      expect(jest.getTimerCount()).toBe(1)
      expect(drainConsoleErrors(RELEASE_FAILURE_MESSAGE, leaseId)).toBe(1)

      const cleanup = await current.binding.destroy()
      expect(cleanup.state).toBe('release-failed')
      expect(routerDestroy).toHaveBeenCalledTimes(1)
      drainConsoleErrors(RELEASE_FAILURE_MESSAGE, leaseId)
      const callsAtDestroy = releaseRenderer.mock.calls.length

      jest.advanceTimersByTime(1000)
      await flushAsyncWork()

      expect(releaseRenderer.mock.calls.length).toBe(callsAtDestroy)
      expect(jest.getTimerCount()).toBe(0)
      expect(current.binding.renderers.has(leaseId)).toBe(true)
    } finally {
      jest.useRealTimers()
    }
  })
})
