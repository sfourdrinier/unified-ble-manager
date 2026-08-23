const { runScanPathHarness } = require('../../scripts/performance/scan-path-harness')

describe('PR9 measured scan-path harness', () => {
  test('reports deterministic path counters and real p50/p95 timing with blocked native evidence', () => {
    const report = runScanPathHarness({ samples: 3, events: 24 })

    expect(report.schema).toBe('unified-ble-scan-path-performance/v1')
    expect(report.proof).toEqual({ scope: 'deterministic-scan-path-model', claim: 'model-only' })
    expect(report.schedule).toEqual({ clock: 'deterministic-virtual-time', eventCount: 24 })
    expect(report.matrix).toEqual(
      expect.arrayContaining([
        expect.objectContaining({ id: 'native-only-1-peer', peerCount: 1, evidence: 'deterministic-model-only' }),
        expect.objectContaining({ id: 'residual-only-10-peers', peerCount: 10 }),
        expect.objectContaining({ id: 'mixed-50-peers', peerCount: 50 })
      ])
    )
    expect(report.metrics.callbacks).toEqual(expect.objectContaining({ status: 'measured', count: 16 }))
    expect(report.metrics.residualMatcherEvaluations).toEqual(
      expect.objectContaining({ status: 'measured', count: 16 })
    )
    expect(report.metrics.overflow).toEqual(expect.objectContaining({ status: 'measured', droppedItems: 8 }))
    expect(report.latency).toEqual(
      expect.objectContaining({
        status: 'measured',
        p50NanosecondsPerEvent: expect.any(Number),
        p95NanosecondsPerEvent: expect.any(Number)
      })
    )
    expect(report.latency.p95NanosecondsPerEvent).toBeGreaterThanOrEqual(report.latency.p50NanosecondsPerEvent)
    expect(report.evidence).toEqual(
      expect.arrayContaining([
        expect.objectContaining({
          metric: 'native-radio-callback-latency',
          status: 'blocked',
          reason: expect.stringContaining('model')
        })
      ])
    )
  })
})
