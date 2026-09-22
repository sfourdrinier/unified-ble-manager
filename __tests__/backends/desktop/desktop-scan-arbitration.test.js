'use strict'

// Finding 209: one manager admits one physical scan. A second scan while the
// first is still open is refused with scan.already-active, and the refusal
// names the active scan so the cause is diagnosable. Stopping the first scan
// admits the next one (the h10-capture advertisement/find sequencing).

const { openBackend, scanOptions } = require('../../helpers/desktop-rust-core-harness')

jest.setTimeout(30000)

const PLATFORMS = ['bluez', 'corebluetooth', 'winrt']

function codeOf(error) {
  return error.code ?? error.normalized?.code ?? null
}

function platformOf(error) {
  return (error.normalized ?? error).platform ?? null
}

describe('finding 209: a second scan while one is open is refused and diagnosable', () => {
  test.each(PLATFORMS)('%s: the refusal is scan.already-active and names the active scan', async platform => {
    const { backend } = await openBackend(platform)
    try {
      const first = await backend.scanner.start(scanOptions(), 'client-1')
      const blocked = await backend.scanner.start(scanOptions(), 'client-1').then(
        () => {
          throw new Error('expected the second scan to be refused')
        },
        error => error
      )
      expect(codeOf(blocked)).toBe('scan.already-active')
      const platformDetail = platformOf(blocked)
      expect(platformDetail).not.toBeNull()
      expect(JSON.stringify(platformDetail.metadata ?? {})).toContain(String(first.scanSessionId))
      expect((await first.stop()).state).toBe('released')
      // The sequencing the fixed h10-capture uses: stop, then the next scan.
      const second = await backend.scanner.start(scanOptions(), 'client-1')
      expect((await second.stop()).state).toBe('released')
    } finally {
      await backend.destroy()
    }
  })
})
