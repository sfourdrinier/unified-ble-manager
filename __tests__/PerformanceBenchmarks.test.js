// __tests__/PerformanceBenchmarks.test.js

const childProcess = require('node:child_process')
const fs = require('node:fs')
const os = require('node:os')
const path = require('node:path')

const root = path.resolve(__dirname, '..')

describe('production performance benchmark harness', () => {
  test('measures the built binary, stream, IPC, and trace paths without inventing native proof', () => {
    const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'unified-ble-performance-test-'))
    const outputPath = path.join(directory, 'report.json')
    try {
      childProcess.execFileSync(
        process.execPath,
        [
          'scripts/performance/run-performance-suite.js',
          '--samples',
          '3',
          '--target-ms',
          '2',
          '--payload-sizes',
          '0,20,244',
          '--output',
          outputPath
        ],
        { cwd: root, encoding: 'utf8', stdio: 'pipe' }
      )

      const report = JSON.parse(fs.readFileSync(outputPath, 'utf8'))
      expect(report).toMatchObject({
        schema: 'unified-ble-performance-report/v1',
        package: { name: 'unified-ble-manager', version: require('../package.json').version },
        baseline: {
          id: 'unified-ble-pr8-deterministic-performance-v1',
          scope: 'deterministic-core',
          proofLevel: 'deterministic-core',
          nativeHostSeparate: true
        },
        nativeHost: {
          status: 'not-run',
          proofLevel: 'none'
        }
      })
      expect([...new Set(report.measurements.map(measurement => measurement.category))].sort()).toEqual([
        'binary-control-codec',
        'bounded-queue-memory-overflow',
        'bounded-stream',
        'byte-ownership-copy',
        'cleanup-10000-operations',
        'control-ipc-snapshot',
        'cross-connection-dispatch-admission',
        'ipc-copy',
        'long-write-chunk-planning',
        'notification-sustained-ingress',
        'per-connection-queue-scheduling-fairness',
        'service-change-invalidation-stream-close',
        'trace',
        'write-with-response-operation',
        'write-without-response-readiness'
      ])
      expect(report.measurements).toHaveLength(23)
      expect(report.measurements.filter(measurement => measurement.id.startsWith('pr8-'))).toHaveLength(10)
      expect(report.measurements.map(measurement => measurement.id)).toEqual(
        expect.arrayContaining([
          'pr8-per-connection-queue-scheduling-fairness',
          'pr8-cross-connection-dispatch-admission',
          'pr8-write-with-response-operation',
          'pr8-write-without-response-readiness',
          'pr8-notification-sustained-ingress',
          'pr8-long-write-chunk-planning',
          'pr8-control-ipc-snapshot',
          'pr8-bounded-queue-memory-overflow',
          'pr8-service-change-invalidation-stream-close',
          'pr8-cleanup-10000-operations'
        ])
      )
      for (const measurement of report.measurements) {
        expect(measurement.samples).toHaveLength(3)
        expect(measurement.summary.p50OperationsPerSecond).toBeGreaterThan(0)
        expect(measurement.summary.p50NanosecondsPerOperation).toBeGreaterThan(0)
        expect(measurement.summary.p95NanosecondsPerOperation).toBeGreaterThanOrEqual(
          measurement.summary.p50NanosecondsPerOperation
        )
        expect(measurement.ownership).toBeTruthy()
        expect(measurement.baselineId).toBe('unified-ble-pr8-deterministic-performance-v1')
        expect(measurement.proofLevel).toBe('deterministic-core')
        expect(measurement.ownershipMetadata).toMatchObject({ scope: 'deterministic-core', cleanup: 'before-return' })
      }
    } finally {
      fs.rmSync(directory, { recursive: true, force: true })
    }
  })

  test('fails closed when native-host proof is required but no native report was supplied', () => {
    expect(() =>
      childProcess.execFileSync(
        process.execPath,
        [
          'scripts/performance/run-performance-suite.js',
          '--samples',
          '3',
          '--target-ms',
          '2',
          '--payload-sizes',
          '20',
          '--require-native-host'
        ],
        { cwd: root, encoding: 'utf8', stdio: 'pipe' }
      )
    ).toThrow(/required native-host benchmark report was not supplied/)
  })
})
