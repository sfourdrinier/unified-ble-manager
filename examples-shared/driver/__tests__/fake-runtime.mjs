// Deterministic ScenarioRuntime for driver unit tests: manual clock and timers.
export function createFakeRuntime(host = 'test/test') {
  let nowMs = 0
  let nextTimer = 1
  const timers = new Map()
  const logs = []
  return {
    host,
    logs,
    now: () => nowMs,
    schedule(callback, delayMs) {
      const id = nextTimer++
      timers.set(id, { at: nowMs + delayMs, callback })
      return () => timers.delete(id)
    },
    log(scope, message, detail) {
      logs.push({ scope, message, detail })
    },
    advance(ms) {
      nowMs += ms
      for (const [id, timer] of [...timers.entries()].sort((a, b) => a[1].at - b[1].at)) {
        if (timer.at <= nowMs && timers.has(id)) {
          timers.delete(id)
          timer.callback()
        }
      }
    },
    pendingTimers: () => timers.size
  }
}
