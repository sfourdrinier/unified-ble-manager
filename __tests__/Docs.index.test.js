// __tests__/Docs.index.test.js

const { checkDocsIndex } = require('../scripts/docs/check-docs-index')

describe('documentation index and agent-facing docs consistency', () => {
  test('the documentation index and llms.txt cover every document and entrypoint', () => {
    expect(checkDocsIndex()).toEqual([])
  })
})
