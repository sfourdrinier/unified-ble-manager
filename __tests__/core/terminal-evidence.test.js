const {
  TerminalEvidenceAdmission,
  TerminalEvidenceRecordTooLargeError
} = require('../../src/core/terminal-evidence')
const { CoreBoundedStream } = require('../../src/core/bounded-stream')

function bytes(length) {
  return new Uint8Array(length).fill(7)
}

describe('TerminalEvidenceAdmission', () => {
  test('reserves before activation and publishes only a committed exact record', async () => {
    const calls = []
    const admission = new TerminalEvidenceAdmission({
      reserve: async request => {
        calls.push(['reserve', request])
        return {
          maximumBytes: request.maximumBytes,
          commit: async record => {
            calls.push(['append', record])
            return { ref: 'terminal-1', byteLength: record.canonicalBytes.byteLength, digest: record.digest, formatVersion: 1, durability: 'committed' }
          },
          release: async () => calls.push(['release'])
        }
      }
    }, { producer: 'scan', maximumBytes: 32, formatVersion: 1, scope: { managerId: 'manager', operationId: 'scan-1', incarnation: '1' } })

    await admission.reserve()
    expect(calls).toEqual([['reserve', { producer: 'scan', maximumBytes: 32, formatVersion: 1, scope: { managerId: 'manager', operationId: 'scan-1', incarnation: '1' } }]])
    await expect(admission.commit({ canonicalBytes: bytes(32), digest: 'sha256:test' })).resolves.toEqual({ ref: 'terminal-1', byteLength: 32, digest: 'sha256:test', formatVersion: 1, durability: 'committed' })
    expect(calls[1][1].canonicalBytes).toEqual(bytes(32))
    await admission.release()
    expect(calls[2]).toEqual(['release'])
  })

  test('does not call a host sink with a record exceeding the pre-reserved bound', async () => {
    const commit = jest.fn()
    const admission = new TerminalEvidenceAdmission({
      reserve: async () => ({ maximumBytes: 3, commit, release: async () => {} })
    }, { producer: 'subscription', maximumBytes: 3, formatVersion: 1, scope: { managerId: 'manager', operationId: 'subscription-1', incarnation: '1' } })

    await admission.reserve()
    await expect(admission.commit({ canonicalBytes: bytes(4), digest: 'sha256:too-large' })).rejects.toBeInstanceOf(TerminalEvidenceRecordTooLargeError)
    expect(commit).not.toHaveBeenCalled()
  })

  test('keeps refusal explicit and does not pretend an uncommitted record was retained', async () => {
    const admission = new TerminalEvidenceAdmission({
      reserve: async () => ({
        maximumBytes: 5,
        commit: async () => ({ kind: 'refused', reason: 'quota-exhausted' }),
        release: async () => {}
      })
    }, { producer: 'scan', maximumBytes: 5, formatVersion: 1, scope: { managerId: 'manager', operationId: 'scan-1', incarnation: '1' } })

    await admission.reserve()
    await expect(admission.commit({ canonicalBytes: bytes(5), digest: 'sha256:refused' })).resolves.toEqual({ kind: 'refused', reason: 'quota-exhausted' })
  })

  test('refuses append until the producer has acquired its reservation', async () => {
    const admission = new TerminalEvidenceAdmission({
      reserve: async () => ({ maximumBytes: 1, commit: async () => ({ ref: 'x', byteLength: 1, digest: 'sha256:x', formatVersion: 1, durability: 'committed' }), release: async () => {} })
    }, { producer: 'scan', maximumBytes: 1, formatVersion: 1, scope: { managerId: 'manager', operationId: 'scan-1', incarnation: '1' } })
    await expect(admission.commit({ canonicalBytes: bytes(1), digest: 'sha256:x' })).rejects.toThrow('terminal-evidence.reservation-required')
  })

  test('serializes concurrent reserve calls into one host reservation', async () => {
    let resolve
    const reserve = jest.fn(() => new Promise(done => { resolve = done }))
    const admission = new TerminalEvidenceAdmission({ reserve }, { producer: 'scan', maximumBytes: 1, formatVersion: 1, scope: { managerId: 'manager', operationId: 'scan-1', incarnation: '1' } })
    const first = admission.reserve()
    const second = admission.reserve()
    expect(reserve).toHaveBeenCalledTimes(1)
    resolve({ maximumBytes: 1, commit: async () => ({ ref: 'x', byteLength: 1, digest: 'sha256:x', formatVersion: 1, durability: 'committed' }), release: async () => {} })
    await Promise.all([first, second])
  })

  test('retains ownership after release failure so a later release can retry', async () => {
    let attempts = 0
    const admission = new TerminalEvidenceAdmission({
      reserve: async () => ({ maximumBytes: 1, commit: async () => ({ ref: 'x', byteLength: 1, digest: 'sha256:x', formatVersion: 1, durability: 'committed' }), release: async () => { attempts += 1; if (attempts === 1) throw new Error('disk offline') } })
    }, { producer: 'scan', maximumBytes: 1, formatVersion: 1, scope: { managerId: 'manager', operationId: 'scan-1', incarnation: '1' } })
    await admission.reserve()
    await expect(admission.release()).rejects.toThrow('disk offline')
    await expect(admission.commit({ canonicalBytes: bytes(1), digest: 'sha256:x' })).resolves.toMatchObject({ ref: 'x' })
    await admission.release()
    expect(attempts).toBe(2)
  })

  test('hydrates a referenced terminal asynchronously before iterator delivery', async () => {
    const stream = new CoreBoundedStream({ itemCapacity: 1, byteCapacity: 8, reservedControlCapacity: 1 }, 'error')
    stream.setTerminalEvidenceHydrator(async terminal => ({ ...terminal, error: { code: 'platform.failure', domain: 'scan', operation: 'scan', retryability: 'never' } }))
    stream.finishWithReason('source-failed', null, { ref: 'r', byteLength: 1, digest: 'sha256:r', formatVersion: 1, durability: 'committed', scope: { managerId: 'm', operationId: 'o', incarnation: 'i' } })
    await expect(stream[Symbol.asyncIterator]().next()).resolves.toMatchObject({ value: { kind: 'terminal', error: { code: 'platform.failure' } } })
  })
  test('preserves a later source failure error and committed evidence after earlier source loss', async () => {
    const stream = new CoreBoundedStream({ itemCapacity: 1, byteCapacity: 256, reservedControlCapacity: 1 }, 'error')
    stream.observeSourceLoss({ kind: 'source-loss', scope: 'host-global', source: 'notification-ingress', lostRecords: 1 })
    const error = { code: 'platform.failure', domain: 'scan', operation: 'scan.source-ended', retryability: 'never' }
    const evidence = { ref: 'later-terminal', byteLength: 1, digest: 'sha256:later-terminal', formatVersion: 1, durability: 'committed', scope: { managerId: 'm', operationId: 'o', incarnation: 'i' } }
    stream.finishWithReason('source-failed', error, evidence)
    await expect(stream[Symbol.asyncIterator]().next()).resolves.toMatchObject({
      value: { kind: 'terminal', reason: 'source-failed', error, evidence }
    })
  })
  test('shares in-flight terminal hydration across readers without premature completion', async () => {
    const stream = new CoreBoundedStream({ itemCapacity: 1, byteCapacity: 8, reservedControlCapacity: 1 }, 'error')
    let resolve
    stream.setTerminalEvidenceHydrator(terminal => new Promise(done => { resolve = () => done({ ...terminal, error: { code: 'platform.failure', domain: 'scan', operation: 'scan', retryability: 'never' } }) }))
    stream.finishWithReason('source-failed', null, { ref: 'r', byteLength: 1, digest: 'sha256:r', formatVersion: 1, durability: 'committed', scope: { managerId: 'm', operationId: 'o', incarnation: 'i' } })
    const first = stream[Symbol.asyncIterator]()
    const second = stream[Symbol.asyncIterator]()
    const pendingFirst = first.next()
    const duplicateFirst = first.next()
    const pendingSecond = second.next()
    resolve()
    await expect(pendingFirst).resolves.toMatchObject({ done: false, value: { kind: 'terminal' } })
    await expect(duplicateFirst).resolves.toEqual({ done: true, value: undefined })
    await expect(pendingSecond).resolves.toMatchObject({ done: false, value: { kind: 'terminal' } })
    await expect(first.next()).resolves.toMatchObject({ done: true })
    await expect(second.next()).resolves.toMatchObject({ done: true })
  })
  test('does not publish a hydrated terminal after iterator return', async () => {
    const stream = new CoreBoundedStream({ itemCapacity: 1, byteCapacity: 8, reservedControlCapacity: 1 }, 'error')
    let resolve
    stream.setTerminalEvidenceHydrator(terminal => new Promise(done => { resolve = () => done(terminal) }))
    stream.finishWithReason('source-failed', null, { ref: 'r', byteLength: 1, digest: 'sha256:r', formatVersion: 1, durability: 'committed', scope: { managerId: 'm', operationId: 'o', incarnation: 'i' } })
    const iterator = stream[Symbol.asyncIterator]()
    const pending = iterator.next()
    await iterator.return()
    resolve()
    await expect(pending).resolves.toEqual({ done: true, value: undefined })
  })
  test('does not release retained evidence while hydration is pending', async () => {
    const stream = new CoreBoundedStream({ itemCapacity: 1, byteCapacity: 8, reservedControlCapacity: 1 }, 'error')
    let resolve
    let drained = 0
    stream.onRetainedValuesDrained(() => { drained += 1 })
    stream.setTerminalEvidenceHydrator(terminal => new Promise(done => { resolve = () => done(terminal) }))
    stream.finishWithReason('source-failed', null, { ref: 'r', byteLength: 1, digest: 'sha256:r', formatVersion: 1, durability: 'committed', scope: { managerId: 'm', operationId: 'o', incarnation: 'i' } })
    const pending = stream[Symbol.asyncIterator]().next()
    expect(drained).toBe(0)
    resolve()
    await pending
    expect(drained).toBe(1)
  })
  test('dispose fences pending hydration for current and later readers', async () => {
    const stream = new CoreBoundedStream({ itemCapacity: 1, byteCapacity: 8, reservedControlCapacity: 1 }, 'error')
    let resolve
    stream.setTerminalEvidenceHydrator(terminal => new Promise(done => { resolve = () => done(terminal) }))
    stream.finishWithReason('source-failed', null, { ref: 'r', byteLength: 1, digest: 'sha256:r', formatVersion: 1, durability: 'committed', scope: { managerId: 'm', operationId: 'o', incarnation: 'i' } })
    const iterator = stream[Symbol.asyncIterator]()
    const pending = iterator.next()
    stream.dispose()
    resolve()
    await expect(pending).resolves.toEqual({ done: true, value: undefined })
    await expect(stream[Symbol.asyncIterator]().next()).resolves.toEqual({ done: true, value: undefined })
  })
  test('previews close counters without publishing or changing queue ownership', async () => {
    const stream = new CoreBoundedStream({ itemCapacity: 2, byteCapacity: 10, reservedControlCapacity: 1 }, 'error')
    stream.emit('a', 3)
    expect(stream.previewTerminal('closed')).toMatchObject({ kind: 'terminal', reason: 'closed', droppedItems: 1, droppedBytes: 3 })
    expect((await stream[Symbol.asyncIterator]().next()).value).toMatchObject({ kind: 'value', value: 'a' })
    stream.closeWithReason('closed')
    await expect(stream[Symbol.asyncIterator]().next()).resolves.toMatchObject({ value: { kind: 'terminal', droppedItems: 0 } })
  })
  test('retains terminal evidence for retry when hydration fails', async () => {
    const stream = new CoreBoundedStream({ itemCapacity: 1, byteCapacity: 8, reservedControlCapacity: 1 }, 'error')
    let attempts = 0
    stream.setTerminalEvidenceHydrator(async terminal => { attempts += 1; if (attempts === 1) throw new Error('offline'); return { ...terminal, error: { code: 'platform.failure', domain: 'scan', operation: 'scan', retryability: 'never' } } })
    stream.finishWithReason('source-failed', null, { ref: 'r', byteLength: 1, digest: 'sha256:r', formatVersion: 1, durability: 'committed', scope: { managerId: 'm', operationId: 'o', incarnation: 'i' } })
    const iterator = stream[Symbol.asyncIterator]()
    await expect(iterator.next()).rejects.toThrow('offline')
    await expect(iterator.next()).resolves.toMatchObject({ value: { kind: 'terminal', error: { code: 'platform.failure' } } })
  })
  test('restores a pending terminal after a synchronous hydrator throw and retries its exact receipt', async () => {
    const stream = new CoreBoundedStream({ itemCapacity: 1, byteCapacity: 8, reservedControlCapacity: 1 }, 'error')
    let attempts = 0
    stream.setTerminalEvidenceHydrator(terminal => {
      attempts += 1
      if (attempts === 1) throw new Error('synchronous offline')
      return Promise.resolve({ ...terminal, error: { code: 'platform.failure', domain: 'scan', operation: 'scan', retryability: 'never' } })
    })
    const iterator = stream[Symbol.asyncIterator]()
    const pending = iterator.next()
    stream.finishWithReason('source-failed', null, { ref: 'exact-retry', byteLength: 1, digest: 'sha256:exact-retry', formatVersion: 1, durability: 'committed', scope: { managerId: 'm', operationId: 'o', incarnation: 'i' } })
    await expect(pending).rejects.toThrow('synchronous offline')
    await expect(iterator.next()).resolves.toMatchObject({ value: { kind: 'terminal', evidence: { ref: 'exact-retry' }, error: { code: 'platform.failure' } } })
    expect(attempts).toBe(2)
  })
  test('cancelling a pending hydration suppresses that reader without retiring the receipt', async () => {
    const stream = new CoreBoundedStream({ itemCapacity: 1, byteCapacity: 8, reservedControlCapacity: 1 }, 'error')
    let resolve
    let attempts = 0
    stream.setTerminalEvidenceHydrator(terminal => {
      attempts += 1
      return new Promise(done => { resolve = () => done({ ...terminal, error: { code: 'platform.failure', domain: 'scan', operation: 'scan', retryability: 'never' } }) })
    })
    const iterator = stream[Symbol.asyncIterator]()
    const pending = iterator.next()
    stream.finishWithReason('source-failed', null, { ref: 'cancel-retry', byteLength: 1, digest: 'sha256:cancel-retry', formatVersion: 1, durability: 'committed', scope: { managerId: 'm', operationId: 'o', incarnation: 'i' } })
    await iterator.return()
    resolve()
    await expect(pending).resolves.toEqual({ done: true, value: undefined })
    const retry = stream[Symbol.asyncIterator]()
    await expect(retry.next()).resolves.toMatchObject({ value: { kind: 'terminal', evidence: { ref: 'cancel-retry' }, error: { code: 'platform.failure' } } })
    expect(attempts).toBe(1)
  })
  test('a rejected hydration after dispose cannot resurrect its terminal receipt', async () => {
    const stream = new CoreBoundedStream({ itemCapacity: 1, byteCapacity: 8, reservedControlCapacity: 1 }, 'error')
    let reject
    stream.setTerminalEvidenceHydrator(() => new Promise((_resolve, fail) => { reject = fail }))
    stream.finishWithReason('source-failed', null, { ref: 'disposed-rejection', byteLength: 1, digest: 'sha256:disposed-rejection', formatVersion: 1, durability: 'committed', scope: { managerId: 'm', operationId: 'o', incarnation: 'i' } })
    const pending = stream[Symbol.asyncIterator]().next()
    stream.dispose()
    reject(new Error('late read failure'))
    await expect(pending).resolves.toEqual({ done: true, value: undefined })
    await expect(stream[Symbol.asyncIterator]().next()).resolves.toEqual({ done: true, value: undefined })
  })
})
