async function withHostCleanup(host, action) {
  let primary
  let cleanup
  try {
    await action()
  } catch (error) {
    primary = error
  }
  try {
    expect(await host.destroy()).toEqual({ state: 'released', failures: [] })
  } catch (error) {
    cleanup = error
  }
  if (primary !== undefined && cleanup !== undefined)
    throw new AggregateError([primary, cleanup], 'Process-host operation and teardown failed')
  if (primary !== undefined) throw primary
  if (cleanup !== undefined) throw cleanup
}

module.exports = { withHostCleanup }
