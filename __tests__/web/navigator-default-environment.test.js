// __tests__/web/navigator-default-environment.test.js

const {
  createDefaultNavigatorWebBluetoothEnvironment
} = require('../../src/web/navigator-web-bluetooth-boundary')

describe('default navigator Web Bluetooth environment', () => {
  test('visibilitychange reports page-hidden only when the document is hidden', () => {
    const previousDocument = globalThis.document
    const previousWindow = globalThis.window
    const listeners = new Map()
    globalThis.document = {
      visibilityState: 'visible',
      addEventListener(type, listener) {
        listeners.set(type, listener)
      },
      removeEventListener(type) {
        listeners.delete(type)
      }
    }
    globalThis.window = {
      addEventListener() {},
      removeEventListener() {}
    }
    try {
      const environment = createDefaultNavigatorWebBluetoothEnvironment()
      const reasons = []
      const stop = environment.addPageLifecycleListener(reason => {
        reasons.push(reason)
      })
      listeners.get('visibilitychange')()
      expect(reasons).toEqual([])
      globalThis.document.visibilityState = 'hidden'
      listeners.get('visibilitychange')()
      expect(reasons).toEqual(['page-hidden'])
      stop()
    } finally {
      globalThis.document = previousDocument
      globalThis.window = previousWindow
    }
  })
})
