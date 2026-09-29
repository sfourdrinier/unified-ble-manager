import { test } from 'node:test'
import assert from 'node:assert/strict'
import { exportRecordingFile, persistRecordingDocument } from '../recording-file-export.ts'

test('file adapter forbids overwrite and confirms exact written bytes before reporting its URI', async () => {
  let contents = ''
  const file = {
    uri: 'file:///capture.json',
    exists: true,
    create: options => assert.deepEqual(options, { overwrite: false }),
    write: value => {
      contents = value
    },
    text: async () => contents
  }
  assert.equal(await persistRecordingDocument(file, JSON.stringify(recording)), file.uri)
  await assert.rejects(
    persistRecordingDocument({ ...file, text: async () => 'truncated' }, 'complete'),
    /not verified.*capture.json/
  )
  await assert.rejects(
    persistRecordingDocument(
      {
        ...file,
        write: () => {
          throw new Error('disk full')
        }
      },
      'complete'
    ),
    /disk full/
  )
})

const recording = {
  schemaVersion: 1,
  frames: [{ timestampNs: '18446744073709551615', rawBytes: [0, 255], samples: [{ x: -32768, y: 0, z: 32767 }] }]
}

test('exports the complete JSON file and only reports share-sheet closure, not external success', async () => {
  const calls = []
  const result = await exportRecordingFile(recording, {
    writeDocument: async contents => {
      calls.push(['write', contents])
      return 'file:///recordings/h10.json'
    },
    isSharingAvailable: async () => true,
    shareDocument: async uri => {
      calls.push(['share', uri])
    }
  })
  assert.deepEqual(JSON.parse(calls[0][1]), recording)
  assert.deepEqual(calls[1], ['share', 'file:///recordings/h10.json'])
  assert.deepEqual(result, { uri: 'file:///recordings/h10.json', sharing: 'closed' })
})

test('failed file creation rejects and never opens sharing', async () => {
  await assert.rejects(
    exportRecordingFile(recording, {
      writeDocument: async () => {
        throw new Error('storage full')
      },
      isSharingAvailable: async () => {
        assert.fail('no sharing after failed persistence')
      },
      shareDocument: async () => {
        assert.fail('no sharing after failed persistence')
      }
    }),
    /storage full/
  )
})

test('unavailable sharing retains a truthful local saved-file result', async () => {
  assert.deepEqual(
    await exportRecordingFile(recording, {
      writeDocument: async () => 'file:///local.json',
      isSharingAvailable: async () => false,
      shareDocument: async () => {
        assert.fail('unavailable')
      }
    }),
    { uri: 'file:///local.json', sharing: 'unavailable' }
  )
})

test('sharing refusal and availability errors preserve local file location and exact reason', async () => {
  for (const failureStage of ['availability', 'share']) {
    const result = await exportRecordingFile(recording, {
      writeDocument: async () => 'file:///local.json',
      isSharingAvailable: async () => {
        if (failureStage === 'availability') throw new Error('availability refused')
        return true
      },
      shareDocument: async () => {
        throw new Error('share refused')
      }
    })
    assert.equal(result.uri, 'file:///local.json')
    assert.equal(result.sharing, 'failed')
    assert.match(result.error, /refused/)
  }
})
