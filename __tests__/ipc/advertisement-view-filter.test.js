'use strict'

const { advertisementPassesViewFilter } = require('../../src/ipc/advertisement-view-filter')

function advertisement(overrides = {}) {
  return {
    peerId: 'AA-BB',
    localName: 'Polar H10 1234',
    rssi: -55,
    serviceUuids: ['0000180d-0000-1000-8000-00805f9b34fb'],
    manufacturerData: [{ companyId: 0x006b, data: new Uint8Array([1, 2]) }],
    ...overrides
  }
}

describe('advertisementPassesViewFilter', () => {
  test('empty filter keeps every advertisement', () => {
    expect(advertisementPassesViewFilter(advertisement(), {})).toBe(true)
  })

  test('nameContains matches local name or peer id without regard to case', () => {
    expect(advertisementPassesViewFilter(advertisement(), { nameContains: 'polar' })).toBe(true)
    expect(advertisementPassesViewFilter(advertisement({ localName: null }), { nameContains: 'aa-bb' })).toBe(true)
    expect(advertisementPassesViewFilter(advertisement(), { nameContains: 'movesense' })).toBe(false)
  })

  test('minRssi rejects weaker and unknown signals', () => {
    expect(advertisementPassesViewFilter(advertisement({ rssi: -40 }), { minRssi: -60 })).toBe(true)
    expect(advertisementPassesViewFilter(advertisement({ rssi: -80 }), { minRssi: -60 })).toBe(false)
    expect(advertisementPassesViewFilter(advertisement({ rssi: null }), { minRssi: -90 })).toBe(false)
  })

  test('serviceUuid matches compact or canonical forms', () => {
    expect(advertisementPassesViewFilter(advertisement(), { serviceUuid: '180d' })).toBe(true)
    expect(advertisementPassesViewFilter(advertisement(), { serviceUuid: '180f' })).toBe(false)
  })

  test('manufacturerCompanyId matches advertised company ids', () => {
    expect(advertisementPassesViewFilter(advertisement(), { manufacturerCompanyId: 0x006b })).toBe(true)
    expect(advertisementPassesViewFilter(advertisement(), { manufacturerCompanyId: 0x004c })).toBe(false)
  })

  test('namedOnly drops advertisements with no local name', () => {
    expect(advertisementPassesViewFilter(advertisement({ localName: null }), { namedOnly: true })).toBe(false)
    expect(advertisementPassesViewFilter(advertisement({ localName: '  ' }), { namedOnly: true })).toBe(false)
    expect(advertisementPassesViewFilter(advertisement(), { namedOnly: true })).toBe(true)
  })
})
