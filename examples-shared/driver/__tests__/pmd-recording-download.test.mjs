import assert from 'node:assert/strict'
import test from 'node:test'
import { PmdRecorder } from '../pmd-recording.ts'
import { preparePmdDownload } from '../browser/pmd-recording-download.ts'

test('download contains the original versioned recording and a safe deterministic file name', () => {
  const recorder = new PmdRecorder()
  recorder.start({ label: '../../unsafe 🫀', wallClock: '2026-09-27T00:00:00Z' }, 10)
  recorder.append({ kind: 'packet', peerId: 'sim', generation: '1', atMs: 15, data: { bytesHex: '0200' } })
  recorder.stop(20)
  const recording = recorder.export()
  const download = preparePmdDownload(recording, new Date('2026-09-27T01:02:03Z'))
  assert.equal(download.filename, 'ubm-pmd-2026-09-27T01-02-03-000Z.json')
  assert.deepEqual(JSON.parse(download.contents), recording)
  assert.equal(download.summary.records, 1)
  assert.throws(() => preparePmdDownload({}), /recording/)
})
