const { WebOrderedRecordingSupervisor } = require('../../src/web/web-ordered-recording-supervisor')

test('an abort during authority preparation prevents source installation and enable', async () => {
  const abort = new AbortController()
  let finishPrepare
  const prepare = new Promise(resolve => { finishPrepare = resolve })
  const owner = {
    reserve: jest.fn(() => ({ memberships: () => [{ member: '0', epoch: '1' }] })),
    install: jest.fn(),
    release: jest.fn(async () => {}),
    isCurrent: () => true
  }
  const journal = {
    prepare: jest.fn(() => { abort.abort(); return prepare }),
    open: jest.fn(),
    commit: jest.fn(),
    retire: jest.fn(),
    seal: jest.fn(),
    releaseAuthority: jest.fn(async () => {}),
    classifyFailure: () => 'failure'
  }
  const supervisor = new WebOrderedRecordingSupervisor(owner, journal)
  const waiting = supervisor.start(abort.signal)
  finishPrepare()
  await expect(waiting).rejects.toMatchObject({ name: 'AbortError' })
  await supervisor.retry()
  expect(owner.install).not.toHaveBeenCalled()
  expect(journal.open).not.toHaveBeenCalled()
  expect(journal.releaseAuthority).toHaveBeenCalledTimes(1)
})
