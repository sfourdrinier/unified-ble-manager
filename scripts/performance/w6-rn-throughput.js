// scripts/performance/w6-rn-throughput.js
//
// W6 React Native throughput benchmark entry point. Measures, for 1, 3 and 6
// simultaneous notification streams in the deterministic RN harness (no
// radio): native calls/sec, notifications/sec, drain backlog age, retained
// bytes, and lifecycle-delivery latency. Read-only measurement: the drain
// design is unchanged. Deterministic: fixed sizes and counts, no randomness.
//
// The measurement itself runs under jest (scripts/performance/
// w6-rn-throughput.measure.test.js): the deterministic Rust-core harness is
// only supported in that runtime — under plain Node its wake-to-drain chain
// never fires, so a direct script would report nothing. This entry point
// spawns that file, prints the report, and exits nonzero on any failure
// (including the measurement's own hang guards, which fail loudly instead
// of hanging).
//
// Usage: node scripts/performance/w6-rn-throughput.js [--output <path>]
// (run with the repo toolchain on PATH, as for the other gates).

'use strict'

const childProcess = require('node:child_process')
const fs = require('node:fs')
const os = require('node:os')
const path = require('node:path')

const root = path.resolve(__dirname, '../..')

function parseArguments(raw) {
  let outputPath = null
  for (let index = 0; index < raw.length; index += 1) {
    if (raw[index] === '--output') {
      const value = raw[index + 1]
      if (value === undefined || value.length === 0) throw new Error('usage: w6-rn-throughput.js [--output <path>]')
      outputPath = path.resolve(process.cwd(), value)
      index += 1
    } else {
      throw new Error(`unknown option: ${raw[index]} (usage: w6-rn-throughput.js [--output <path>])`)
    }
  }
  return { outputPath }
}

function main() {
  const { outputPath } = parseArguments(process.argv.slice(2))
  const measureOutput = path.join(
    fs.mkdtempSync(path.join(os.tmpdir(), 'w6-rn-throughput-')),
    'report.json'
  )
  const jestBinary = path.join(root, 'node_modules', '.bin', process.platform === 'win32' ? 'jest.cmd' : 'jest')
  const result = childProcess.spawnSync(
    jestBinary,
    [
      '--config',
      path.join(root, 'jest.config.js'),
      '--roots',
      path.join(root, 'scripts', 'performance'),
      '--runInBand',
      'w6-rn-throughput.measure'
    ],
    {
      cwd: root,
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'pipe'],
      env: { ...process.env, W6_BENCH_OUTPUT: measureOutput },
      timeout: 570000
    }
  )
  const stdout = typeof result.stdout === 'string' ? result.stdout : ''
  const stderr = typeof result.stderr === 'string' ? result.stderr : ''
  if (result.error) throw result.error
  const summary = stdout
    .split('\n')
    .filter(line => line.startsWith('streams='))
    .join('\n')
  if (summary.length > 0) console.log(summary)
  if (result.status !== 0) {
    console.error(stderr.slice(-4000))
    throw new Error(`w6-rn-throughput measurement failed with exit code ${String(result.status)}`)
  }
  const report = JSON.parse(fs.readFileSync(measureOutput, 'utf8'))
  const text = JSON.stringify(report, null, 2)
  if (outputPath !== null) fs.writeFileSync(outputPath, `${text}\n`)
  console.log(text)
}

try {
  main()
} catch (error) {
  console.error(`w6-rn-throughput failed: ${error?.stack ?? String(error)}`)
  process.exitCode = 1
}
