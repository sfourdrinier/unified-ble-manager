const { decodeIpcScanPlatform } = require('../../src/ipc/scan-platform')

test.each(['active', 'passive', 'none'])('IPC scan decoder preserves Windows %s and extended opt-in', mode => {
  expect(decodeIpcScanPlatform({ kind: 'winrt', mode, allowExtendedAdvertisements: true })).toEqual({
    kind: 'winrt',
    mode,
    allowExtendedAdvertisements: true
  })
})

test.each([
  { kind: 'winrt', mode: 'invalid' },
  { kind: 'winrt', allowExtendedAdvertisements: 1 },
  { kind: 'winrt', phy: 'coded' },
  { kind: 'winrt', reportDelayMs: 500 }
])('IPC Windows scan refuses unsupported fields and malformed controls before dispatch', value => {
  expect(() => decodeIpcScanPlatform(value)).toThrow(
    expect.objectContaining({ normalized: expect.objectContaining({ code: 'argument.invalid' }) })
  )
})
