const { withHostCleanup } = require('../../helpers/desktop-process-host-cleanup')

test('process-host fixture teardown preserves both primary and cleanup failures', async () => {
  const primary = new Error('primary native operation failed')
  const cleanup = new Error('native cleanup failed')
  const host = { destroy: jest.fn().mockRejectedValue(cleanup) }
  let failure
  try {
    await withHostCleanup(host, async () => {
      throw primary
    })
  } catch (error) {
    failure = error
  }
  expect(host.destroy).toHaveBeenCalledTimes(1)
  expect(failure).toBeInstanceOf(AggregateError)
  expect(failure.errors).toEqual([primary, cleanup])
})

test.each(['primary', 'cleanup', 'neither'])(
  'process-host fixture reports %s failure without substitution',
  async kind => {
    const failure = new Error(kind)
    const host = {
      destroy:
        kind === 'cleanup'
          ? jest.fn().mockRejectedValue(failure)
          : jest.fn().mockResolvedValue({ state: 'released', failures: [] })
    }
    const pending = withHostCleanup(host, async () => {
      if (kind === 'primary') throw failure
    })
    if (kind === 'neither') await expect(pending).resolves.toBeUndefined()
    else await expect(pending).rejects.toBe(failure)
    expect(host.destroy).toHaveBeenCalledTimes(1)
  }
)
