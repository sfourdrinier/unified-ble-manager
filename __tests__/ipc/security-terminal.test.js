const { createIpcSecurityBackend } = require('../../src/ipc/security')
const { CoreBoundedStream } = require('../../src/core/bounded-stream')
const { contractError } = require('../../src/backend-contract/errors')
const { capacity } = require('../../src/backend-contract/primitives')

test.each(['overflow', 'source-failed'])(
  'ceremony %s terminal aborts outstanding pairing and preserves its cause',
  async reason => {
    let stream, notify
    const nativeCause = contractError('platform.transport', 'platform', 'native.security.event-source').normalized
    let aborted = false
    const ipc = {
      registerStream: jest.fn((_handle, _guard, _limits, policy, onTerminal) => {
        notify = onTerminal
        stream = new CoreBoundedStream(
          { itemCapacity: capacity(1), byteCapacity: capacity(4096), reservedControlCapacity: capacity(1) },
          policy ?? 'drop-oldest'
        )
        return stream
      }),
      route: jest.fn(
        (_command, _payload, _binary, signal) =>
          new Promise((_resolve, reject) =>
            signal.addEventListener(
              'abort',
              () => {
                aborted = true
                reject(contractError('operation.aborted', 'platform', 'native.security.pair'))
              },
              { once: true }
            )
          )
      ),
      closeStream: () => stream.closeWithReason('owner-released')
    }
    const pairing = createIpcSecurityBackend(ipc).pair('peer', {
      signal: null,
      deadline: null,
      transport: 'le',
      protection: 'system-default',
      ceremony: { kind: 'agent', agent: { onChallenge: async () => new Promise(() => {}) } }
    })
    const outcome = expect(pairing).rejects.toMatchObject({
      normalized: reason === 'overflow' ? { code: 'stream.overflow' } : nativeCause
    })
    expect(ipc.registerStream.mock.calls[0][3]).toBe('error')
    stream.finishWithReason(reason, reason === 'source-failed' ? nativeCause : null)
    if (notify) notify(reason, reason === 'source-failed' ? nativeCause : null)
    await outcome
    expect(aborted).toBe(true)
  }
)
