import assert from 'node:assert/strict'
import test from 'node:test'
import { execFileSync, spawnSync } from 'node:child_process'
import { mkdtempSync, writeFileSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { PmdRecorder } from '../pmd-recording.ts'
import { summarizePmdRecording } from '../pmd-recording-analysis.ts'

function frame(timestamp, x) {
  const bytes = Buffer.alloc(16)
  bytes[0] = 2
  bytes.writeBigUInt64LE(timestamp, 1)
  bytes[9] = 1
  bytes.writeInt16LE(x, 10)
  bytes.writeInt16LE(-20, 12)
  bytes.writeInt16LE(1000, 14)
  return bytes.toString('hex')
}

test('comparison re-decodes raw samples, preserves large clocks and excludes first packet from measured rate', () => {
  const recorder = new PmdRecorder()
  recorder.start({ label: 'sim' }, 0)
  for (const [index, x] of [10, 20, 30].entries())
    recorder.append({
      kind: 'packet',
      peerId: 'SIM',
      generation: '1',
      atMs: index * 10,
      data: {
        bytesHex: frame(90071992547409930n + BigInt(index) * 10000000n, x),
        settings: { sampleRateHz: 100, rangeG: 8, resolutionBits: 16 }
      }
    })
  recorder.stop(30)
  const result = summarizePmdRecording(recorder.export())
  assert.equal(result.streams.length, 1)
  assert.equal(result.streams[0].measuredRateHz, 100)
  assert.equal(result.streams[0].samples, 3)
  assert.equal(result.streams[0].firstTimestampNs, '90071992547409930')
  assert.deepEqual(result.streams[0].axes.x, { min: 10, max: 30, mean: 20 })
  assert.equal(result.streams[0].timestampDiscontinuities, 0)
  assert.equal(result.errors.length, 0)
})

test('malformed packets, loss and timestamp gaps are reported, never treated as equivalent captures', () => {
  const recorder = new PmdRecorder()
  recorder.start({}, 0)
  for (const [index, bytesHex] of [frame(100n, 1), frame(20000100n, 2), 'zz'].entries())
    recorder.append({
      kind: 'packet',
      peerId: 'SIM',
      generation: '1',
      atMs: index,
      data: { bytesHex, settings: { sampleRateHz: 100 } }
    })
  recorder.append({ kind: 'loss', peerId: 'SIM', generation: '1', atMs: 4, data: { dropped: 2 } })
  recorder.stop(5)
  const result = summarizePmdRecording(recorder.export())
  assert.equal(result.streams[0].timestampDiscontinuities, 1)
  assert.equal(result.errors.length, 1)
  assert.equal(result.lossRecords.length, 1)
  assert.equal(result.equivalenceEstablished, false)
  assert.throws(() => summarizePmdRecording({}), /recording/)
})

test('CLI compares actual exported files and rejects missing or malformed inputs', () => {
  const directory = mkdtempSync(join(tmpdir(), 'ubm-pmd-compare-'))
  const script = fileURLToPath(new URL('../compare-pmd-recordings.mjs', import.meta.url))
  try {
    const recorder = new PmdRecorder()
    recorder.start({ label: 'fixture' }, 0)
    recorder.stop(1)
    const file = join(directory, 'capture.json')
    writeFileSync(file, JSON.stringify(recorder.export()))
    const result = JSON.parse(execFileSync(process.execPath, [script, file, file], { encoding: 'utf8' }))
    assert.equal(result.captures.length, 2)
    assert.equal(result.captures[0].metadata.label, 'fixture')
    assert.equal(spawnSync(process.execPath, [script], { encoding: 'utf8' }).status, 1)
    writeFileSync(file, '{}')
    const invalid = spawnSync(process.execPath, [script, file, file], { encoding: 'utf8' })
    assert.equal(invalid.status, 1)
    assert.match(invalid.stderr, /invalid PMD recording/)
  } finally {
    rmSync(directory, { recursive: true, force: true })
  }
})
