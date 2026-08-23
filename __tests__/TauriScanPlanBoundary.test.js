'use strict'

const { normalizeScanQuery } = require('../src/public/scan-query')

function query() {
  return normalizeScanQuery({
    anyOf: [
      {
        services: { all: ['180d'] },
        names: { prefixes: ['Polar'] },
        manufacturerData: { any: [{ companyId: 76, dataPrefix: new Uint8Array([1, 2]) }] }
      }
    ]
  })
}

describe('Tauri trusted scan-plan boundary', () => {
  test('serializes a normalized query without undefined values or mutable aliases', () => {
    const { encodeTauriScanQuery } = require('../src/tauri/scan-plan')
    const normalized = query()
    const wire = encodeTauriScanQuery(normalized)

    expect(wire).not.toBe(normalized)
    expect(wire.anyOf[0].services).toEqual({
      any: [],
      all: ['0000180d-0000-1000-8000-00805f9b34fb']
    })
    expect(wire.anyOf[0].manufacturerData.any[0]).toEqual({
      companyId: 76,
      dataPrefix: new Uint8Array([1, 2]),
      mask: null
    })
    expect('mask' in wire.anyOf[0].manufacturerData.any[0]).toBe(true)
    expect(JSON.stringify(wire)).not.toContain('undefined')
    normalized.anyOf[0].manufacturerData.any[0].dataPrefix[0] = 9
    expect(wire.anyOf[0].manufacturerData.any[0].dataPrefix).toEqual(new Uint8Array([1, 2]))
  })

  test('decodes a host-issued diagnostic plan and rejects nativeFilter leakage', () => {
    const { decodeTauriScanPlan, encodeTauriScanQuery } = require('../src/tauri/scan-plan')
    const normalized = query()
    const wireQuery = encodeTauriScanQuery(normalized)
    const plan = decodeTauriScanPlan(
      {
        sourceQuery: wireQuery,
        queryDigest: normalized.digest,
        residualQueryDigest: normalized.digest,
        nativeGuarantee: 'safe-superset',
        native: {
          predicates: [{ clauseSet: 'anyOf', clauseIndex: 0, field: 'services', operator: 'all' }],
          complete: false
        },
        residual: {
          query: wireQuery,
          predicates: [
            { clauseSet: 'anyOf', clauseIndex: 0, field: 'manufacturerData', operator: 'any' },
            { clauseSet: 'anyOf', clauseIndex: 0, field: 'names', operator: 'prefixes' },
            { clauseSet: 'anyOf', clauseIndex: 0, field: 'services', operator: 'all' }
          ],
          complete: true
        },
        unavailable: [],
        limitations: [
          {
            code: 'native-filter-incomplete',
            predicate: { clauseSet: 'anyOf', clauseIndex: 0, field: 'services', operator: 'all' },
            explanation: 'predicate remains in the canonical residual matcher',
            effect: 'performance-only'
          }
        ],
        estimatedCost: 'moderate'
      },
      normalized
    )

    expect(plan.residual.query).toStrictEqual(normalized)
    expect(Object.isFrozen(plan)).toBe(true)
    expect(plan).not.toHaveProperty('nativeFilter')
    expect(() =>
      decodeTauriScanPlan(
        {
          sourceQuery: wireQuery,
          queryDigest: normalized.digest,
          residualQueryDigest: normalized.digest,
          nativeGuarantee: 'safe-superset',
          native: { predicates: [], complete: false },
          residual: { query: wireQuery, predicates: [], complete: true },
          unavailable: [],
          limitations: [],
          estimatedCost: 'high',
          nativeFilter: { serviceUuids: ['forbidden'] }
        },
        normalized
      )
    ).toThrow('nativeFilter')
  })

  test('requires one trusted normalized query for each native scan start', async () => {
    const { TauriBleIpcTransport } = require('../src/tauri/transport')
    const transport = new TauriBleIpcTransport({
      invoke: jest.fn(async () => ({ kind: 'route', payload: { handle: 'scan-1' } })),
      Channel: class {
        onmessage = null
      }
    })
    const scanStart = () =>
      transport.invoke({
        kind: 'route',
        envelope: { command: 'scan.start', payload: { serviceUuids: [], manufacturerData: [], localNamePrefix: null } }
      })

    await expect(transport.withTrustedScanQuery(query(), scanStart)).resolves.toEqual({
      kind: 'route',
      payload: { handle: 'scan-1' }
    })
  })
})
