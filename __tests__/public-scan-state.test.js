const { capacity } = require('../src/backend-contract/primitives')
const { CoreBoundedStream } = require('../src/core/bounded-stream')
const { bindScanSourceTerminal } = require('../src/public/scan-state')

function nativeScanError() {
  return {
    code: 'platform.transport',
    domain: 'stream',
    operation: 'scan.source-failed',
    platform: {
      domain: 'btleplug',
      code: 'native-error',
      safeMessage: 'native scan channel closed',
      metadata: {}
    },
    retryability: 'never'
  }
}

describe('bindScanSourceTerminal', () => {
  test('forwards a structured terminal error to the underlying stream and the observer', async () => {
    const source = new CoreBoundedStream(
      { itemCapacity: capacity(2), byteCapacity: capacity(64), reservedControlCapacity: capacity(1) },
      'drop-oldest'
    )
    const error = nativeScanError()
    const seen = []
    bindScanSourceTerminal(source, (reason, terminalError) => {
      seen.push({ reason, error: terminalError })
    })
    source.finishWithReason('source-failed', error)

    const iterator = source[Symbol.asyncIterator]()
    await expect(iterator.next()).resolves.toMatchObject({
      value: { kind: 'terminal', reason: 'source-failed', error }
    })
    expect(seen).toEqual([{ reason: 'source-failed', error }])
  })

  test('forwards a structured close error to the underlying stream', async () => {
    const source = new CoreBoundedStream(
      { itemCapacity: capacity(2), byteCapacity: capacity(64), reservedControlCapacity: capacity(1) },
      'drop-oldest'
    )
    const error = nativeScanError()
    bindScanSourceTerminal(source, () => undefined)
    source.closeWithReason('source-failed', error)

    const iterator = source[Symbol.asyncIterator]()
    await expect(iterator.next()).resolves.toMatchObject({
      value: { kind: 'terminal', reason: 'source-failed', error }
    })
  })
})
