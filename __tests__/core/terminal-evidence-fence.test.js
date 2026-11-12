const { TerminalEvidenceFence } = require('../../src/core/terminal-evidence-fence')
test('fence drains accepted prefix and rejects post-terminal callbacks', async () => {
  const fence = new TerminalEvidenceFence()
  const seen = []
  let unblock
  expect(fence.enqueue(async () => { seen.push('first'); await new Promise(done => { unblock = done }); seen.push('first-settled') })).toBe(true)
  expect(fence.enqueue(() => seen.push('second'))).toBe(true)
  const sealing = fence.seal()
  expect(fence.enqueue(() => seen.push('after'))).toBe(false)
  await Promise.resolve()
  unblock()
  await sealing
  expect(seen).toEqual(['first', 'first-settled', 'second'])
})
test('one failed accepted callback does not poison later accepted work', async () => {
  const fence = new TerminalEvidenceFence()
  const seen = []
  fence.enqueue(() => { throw new Error('first') })
  fence.enqueue(() => seen.push('later'))
  await expect(fence.seal()).rejects.toThrow('first')
  expect(seen).toEqual(['later'])
})
