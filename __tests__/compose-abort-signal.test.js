const { composeAbortSignal, disposeComposedAbortSignal } = require('../src/public/operation-options')

describe('composeAbortSignal disposal', () => {
  test('aborting one input detaches the listener from the other', () => {
    const outer = new AbortController()
    const inner = new AbortController()
    const added = []
    const removed = []
    const originalAdd = outer.signal.addEventListener.bind(outer.signal)
    const originalRemove = outer.signal.removeEventListener.bind(outer.signal)
    outer.signal.addEventListener = (type, listener, options) => {
      added.push('outer')
      return originalAdd(type, listener, options)
    }
    outer.signal.removeEventListener = (type, listener, options) => {
      removed.push('outer')
      return originalRemove(type, listener, options)
    }
    const innerAdd = inner.signal.addEventListener.bind(inner.signal)
    const innerRemove = inner.signal.removeEventListener.bind(inner.signal)
    inner.signal.addEventListener = (type, listener, options) => {
      added.push('inner')
      return innerAdd(type, listener, options)
    }
    inner.signal.removeEventListener = (type, listener, options) => {
      removed.push('inner')
      return innerRemove(type, listener, options)
    }

    const composed = composeAbortSignal(outer.signal, inner.signal)
    expect(added).toEqual(['outer', 'inner'])
    outer.abort()
    expect(composed.aborted).toBe(true)
    expect(removed).toEqual(['outer', 'inner'])
  })

  test('dispose detaches both listeners after normal settlement', () => {
    const outer = new AbortController()
    const inner = new AbortController()
    const removed = []
    const outerRemove = outer.signal.removeEventListener.bind(outer.signal)
    outer.signal.removeEventListener = (type, listener, options) => {
      removed.push('outer')
      return outerRemove(type, listener, options)
    }
    const innerRemove = inner.signal.removeEventListener.bind(inner.signal)
    inner.signal.removeEventListener = (type, listener, options) => {
      removed.push('inner')
      return innerRemove(type, listener, options)
    }
    const composed = composeAbortSignal(outer.signal, inner.signal)
    disposeComposedAbortSignal(composed)
    disposeComposedAbortSignal(composed)
    expect(removed).toEqual(['outer', 'inner'])
    expect(composed.aborted).toBe(false)
  })
})
