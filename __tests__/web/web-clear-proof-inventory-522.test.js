const { recordingDatabase } = require('./web-recovery-522-helper')
const { createWebRecordingRecovery, webRetiredClearKey } = require('../../src/web/web-recording-recovery')

test('partition proof inventory pages active, archived, attempt and malformed rows without a recording ID', async () => {
  const previous = global.IDBKeyRange
  global.IDBKeyRange = { bound: (lower, upper, lowerOpen = false) => ({ lower, upper, lowerOpen }) }
  try {
    const db = recordingDatabase()
    const receipt = { recordingId: ':ubm-clear-v4:00000000000000000001', version: 3,
      targetRecordingId: 'sample', operationId: 'g1', records: 0, producerIncarnation: 'producer', ownerEpoch: '1', generation: '1' }
    const legacy = { recordingId: ':ubm-clear-v2:other:1', version: 1, operationId: '1', producerIncarnation: 'old', ownerEpoch: '1', generation: '1' }
    const attemptId = 'a'.repeat(64)
    db.seed('readerReceipts', receipt)
    db.seed('readerReceipts', legacy)
    db.seed('readerReceipts', { recordingId: webRetiredClearKey(receipt), version: 1, receipt })
    db.seed('readerReceipts', { recordingId: `:ubm-clear-attempt-v1:${attemptId}`, version: 1, targetRecordingId: 'sample', producerIncarnation: 'producer', attemptId, cleared: true, records: 0 })
    db.seed('readerReceipts', { recordingId: ':ubm-clear-v4:broken', evidence: 'preserve me' })
    const recovery = createWebRecordingRecovery(db)
    let after = null
    const entries = []
    do {
      const page = await recovery.listClearProofs({ after, limit: 2 })
      expect(page.entries.length).toBeLessThanOrEqual(2)
      entries.push(...page.entries)
      after = page.next
    } while (after !== null)
    expect(entries.map(entry => entry.kind).sort()).toEqual(['active', 'active', 'attempt', 'malformed', 'retired'])
    expect(entries.find(entry => entry.kind === 'retired').receipt).toEqual(receipt)
    expect(entries.find(entry => entry.kind === 'malformed').value.evidence).toBe('preserve me')
  } finally { global.IDBKeyRange = previous }
})
