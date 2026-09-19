// scripts/performance/w6-rn-throughput.measure.test.js
//
// W6 React Native throughput measurement, jest leg: runs the one shared
// implementation (scripts/performance/w6-rn-throughput.js) under jest and
// carries the JSON report back to W6_BENCH_OUTPUT. The same implementation
// runs in plain node via the entry point, so both runtimes pin the same
// numbers — no jest-only path left for the drain benchmark (W8).

'use strict'

const fs = require('fs')
const { runThroughputMeasurement, summaryLines } = require('./w6-rn-throughput')

jest.setTimeout(300000)

describe('W6 RN throughput (deterministic harness)', () => {
  test('measures 1, 3 and 6 notification streams', async () => {
    const outputPath = process.env.W6_BENCH_OUTPUT
    if (typeof outputPath !== 'string' || outputPath.length === 0) {
      throw new Error('W6_BENCH_OUTPUT must name the report file')
    }
    const report = await runThroughputMeasurement()
    fs.writeFileSync(outputPath, `${JSON.stringify(report, null, 2)}\n`)
    for (const line of summaryLines(report)) console.log(line)
  })
})
