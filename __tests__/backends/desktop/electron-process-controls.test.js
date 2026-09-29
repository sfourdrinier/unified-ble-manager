const { createNativeContinuationControl } = require('../../../src/backends/desktop/native-continuation-controller')
const { createProcessControls } = require('../../../example-electron/driver/process-controls.cjs')

test('idle Electron controls decode truthfully without acquiring a process host or acknowledging', async () => {
  let acquisitions = 0
  const session = {
    allocatedHost: async () => null,
    processHost: async () => {
      acquisitions++
      throw new Error('unexpected radio')
    }
  }
  const prepareDirectory = jest.fn(async () => {})
  const control = createNativeContinuationControl(createProcessControls(session, '/private/app', prepareDirectory))
  expect(await control.status()).toBeNull()
  expect(await control.claim()).toEqual({
    selectors: [],
    values: [],
    streamEnds: [],
    control: [],
    controlLost: 0,
    afterCutoffLoss: { items: 0, bytes: 0 },
    disposed: false,
    disposeFailure: null
  })
  expect(acquisitions).toBe(0)
  expect(prepareDirectory).not.toHaveBeenCalled()
})

test('recording execution configures trusted storage before raw execute; refused configuration prevents execution', async () => {
  const calls = []
  let refuse = false
  const host = {
    continuation: {
      recordings: async directory => {
        calls.push(['configure', directory])
        if (refuse) throw new Error('store refused')
      }
    },
    continuationAccess: {
      execute: async function (...args) {
        expect(this).toBe(host.continuationAccess)
        calls.push(['execute', ...args])
        return 'envelope'
      }
    }
  }
  const access = createProcessControls({ processHost: async () => host }, '/trusted/store', async () => {
    calls.push(['prepare-directory'])
  })
  const declaration = JSON.stringify({ recording: { id: 'test' } })
  expect(await access.execute('peer', declaration)).toBe('envelope')
  expect(calls).toEqual([['prepare-directory'], ['configure', '/trusted/store'], ['execute', 'peer', declaration]])
  refuse = true
  await expect(access.execute('peer', declaration)).rejects.toThrow('store refused')
  expect(calls.slice(-2)).toEqual([['prepare-directory'], ['configure', '/trusted/store']])
  expect(calls.filter(item => item[0] === 'execute')).toHaveLength(1)
})

test('retained raw prepare/ACK remains accessible without new admission', async () => {
  const calls = []
  const access = {
    prepareClaim: async (...args) => {
      calls.push(['prepare', ...args])
      return 'prepared'
    },
    acknowledgeClaim: async (...args) => {
      calls.push(['ack', ...args])
      return 'acknowledged'
    }
  }
  const prepareDirectory = jest.fn(async () => {})
  const controls = createProcessControls(
    { allocatedHost: async () => ({ continuationAccess: access }) },
    '/private',
    prepareDirectory
  )
  expect(await controls.prepareClaim(2, 100)).toBe('prepared')
  expect(calls).toEqual([['prepare', 2, 100]])
  expect(await controls.acknowledgeClaim('token')).toBe('acknowledged')
  expect(prepareDirectory).not.toHaveBeenCalled()
})
