'use strict'

/** One navigation retry per window lifetime, never a new process/radio owner. */
function installRendererRecovery({
  window,
  load,
  isShuttingDown,
  report,
  timeoutMs = 30000,
  schedule = action => {
    const timer = setTimeout(action, 0)
    return () => clearTimeout(timer)
  }
}) {
  const sender = window.webContents
  let attempted = false
  let revision = 0
  let pending = null
  let running = false
  let stopped = false
  let cancelScheduled = null
  const available = () => !stopped && !window.isDestroyed() && !isShuttingDown()
  const recover = (manual = false) => {
    if (!available() || pending === null || running || cancelScheduled !== null) return false
    const details = pending
    if (attempted && !manual) {
      report({
        state: 'failed',
        details,
        error: new Error('Renderer recovery exhausted; main owner and backlog retained')
      })
      return false
    }
    report({ state: 'scheduled', details })
    // Never navigate synchronously while Electron is tearing down the renderer.
    cancelScheduled = schedule(async () => {
      cancelScheduled = null
      if (!available()) return
      attempted = true
      pending = null
      running = true
      const current = ++revision
      let timer
      try {
        await Promise.race([
          Promise.resolve().then(load),
          new Promise((_resolve, reject) => {
            timer = setTimeout(
              () => reject(new Error('Renderer recovery deadline exceeded; main owner and backlog retained')),
              timeoutMs
            )
          })
        ])
        if (available() && current === revision) report({ state: 'reloaded', details })
      } catch (error) {
        pending ??= details
        if (!stopped && current === revision) report({ state: 'failed', details, error })
      } finally {
        clearTimeout(timer)
        running = false
      }
    })
    return true
  }
  const gone = (_event, details) => {
    if (stopped || window.isDestroyed() || details.reason === 'clean-exit') return
    pending = details
    revision += 1
    if (running)
      report({
        state: 'failed',
        details,
        error: new Error('Renderer terminated during recovery; explicit retry required')
      })
    else recover()
  }
  const dispose = () => {
    if (stopped) return
    stopped = true
    cancelScheduled?.()
    cancelScheduled = null
    sender.removeListener('render-process-gone', gone)
    window.removeListener('closed', dispose)
  }
  sender.on('render-process-gone', gone)
  window.on('closed', dispose)
  return { dispose, resume: () => recover(), retry: () => recover(true) }
}

module.exports = { installRendererRecovery }
