// Optional cross-version integration fixture: provide an extracted v5.2.1 src
// tree and the claim emitted by continuation_adapter's synthetic native test.
const fs = require('fs')
const path = require('path')
const root = process.env.UBM_521_DECODER_ROOT
const claimPath = process.env.UBM_521_STAGED_CLAIM
test('the strict v5.2.1 decoder rejects a real staged native value with provenance', () => {
  if (!root || !claimPath) throw new Error('Set UBM_521_DECODER_ROOT and UBM_521_STAGED_CLAIM')
  const { aggregateContinuationClaim } = require(path.join(root, 'src/backends/reactnative/react-native-continuation-claim'))
  const staged = JSON.parse(fs.readFileSync(claimPath, 'utf8'))
  const records = JSON.parse(staged.batches[0]).records
  expect(records.some(record => record.t === 'value' && typeof record.provenance === 'string')).toBe(true)
  expect(() => aggregateContinuationClaim(staged)).toThrow()
  const legacyShape = JSON.parse(JSON.stringify(staged))
  legacyShape.batches = legacyShape.batches.map(text => {
    const batch = JSON.parse(text)
    for (const record of batch.records) {
      delete record.provenance
      delete record.sourceGeneration
      delete record.sourceOrdinal
    }
    return JSON.stringify(batch)
  })
  expect(aggregateContinuationClaim(legacyShape).values).toHaveLength(1)
})
