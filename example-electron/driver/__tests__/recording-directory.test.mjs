import { test } from 'node:test'
import assert from 'node:assert/strict'
import { createRequire } from 'node:module'
import { mkdtemp, stat, rm, writeFile } from 'node:fs/promises'
import os from 'node:os'
import path from 'node:path'
const require = createRequire(import.meta.url)
const { createRecordingDirectory } = require('../recording-directory.cjs')
const { createProcessControls } = require('../process-controls.cjs')
const { createProcessSession } = require('../process-session.cjs')

test('trusted directory is lazily created recursively with private POSIX mode', async () => {
  const root = await mkdtemp(path.join(os.tmpdir(), 'ubm-electron-directory-'))
  try {
    const directory = path.join(root, 'recordings', 'corebluetooth')
    const prepare = createRecordingDirectory(directory)
    await assert.rejects(stat(directory), { code: 'ENOENT' })
    await Promise.all([prepare(), prepare()])
    assert.equal((await stat(directory)).isDirectory(), true)
    if (process.platform !== 'win32') assert.equal((await stat(directory)).mode & 0o777, 0o700)
    assert.throws(() => createRecordingDirectory('../untrusted'), /absolute/)
  } finally {
    await rm(root, { recursive: true, force: true })
  }
})

test('failed directory preparation preserves the error and is retryable/coalesced', async () => {
  const failure = new Error('permission denied')
  let attempts = 0
  const prepare = createRecordingDirectory('/trusted/recordings', async (directory, options) => {
    assert.equal(directory, '/trusted/recordings')
    assert.deepEqual(options, { recursive: true, mode: 0o700 })
    if (++attempts === 1) throw failure
  })
  await Promise.all([prepare(), prepare()].map(promise => assert.rejects(promise, error => error === failure)))
  assert.equal(attempts, 1)
  await Promise.all([prepare(), prepare()])
  assert.equal(attempts, 2)
})

test('live preparation refusal acquires no radio and never dispatches native execute', async () => {
  let opened = 0
  const failure = new Error('private directory refused')
  const control = createProcessControls(
    {
      processHost: async () => {
        opened++
        throw new Error('must not open')
      }
    },
    '/trusted/recordings',
    async () => {
      throw failure
    }
  )
  await assert.rejects(control.execute('peer', JSON.stringify({ recording: { id: 'r' } })), error => error === failure)
  assert.equal(opened, 0)
})

test('existing non-directory path is refused rather than weakening storage validation', async () => {
  const root = await mkdtemp(path.join(os.tmpdir(), 'ubm-electron-directory-'))
  try {
    const target = path.join(root, 'not-directory')
    await writeFile(target, 'owned test fixture')
    await assert.rejects(createRecordingDirectory(target)())
  } finally {
    await rm(root, { recursive: true, force: true })
  }
})

test('shutdown during directory preparation prevents late radio acquisition', async () => {
  let finish
  let opens = 0
  const prepared = new Promise(resolve => {
    finish = resolve
  })
  const session = createProcessSession({
    createProcessHost: async () => {
      opens++
      throw new Error('must not open')
    }
  })
  const control = createProcessControls(session, '/trusted/recordings', () => prepared)
  const execute = assert.rejects(control.execute('peer', JSON.stringify({ recording: { id: 'r' } })), /closed/)
  assert.equal((await session.destroy()).state, 'released')
  finish()
  await execute
  assert.equal(opens, 0)
})
