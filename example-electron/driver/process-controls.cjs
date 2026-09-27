'use strict'

const envelope = value => JSON.stringify({ ok: true, value })

/** Non-allocating inspection of application ownership. An absent host is not a
 * successful native disposal: the empty answer deliberately says disposed=false.
 * Existing native envelopes are never decoded/ACKed in main. */
function createProcessControls(session, directory, prepareDirectory) {
  return Object.freeze({
    async execute(peerId, declarationJson) {
      const declaration = JSON.parse(declarationJson)
      const recording = declaration !== null && typeof declaration === 'object' && declaration.recording !== undefined
      if (recording) await prepareDirectory()
      const host = await session.processHost()
      if (recording) {
        await host.continuation.recordings(directory)
      }
      return host.continuationAccess.execute(peerId, declarationJson)
    },
    async describeBacklog() {
      const host = await session.allocatedHost()
      return host === null ? envelope(null) : host.continuationAccess.describeBacklog()
    },
    async prepareClaim(maxItems, maxBytes) {
      const host = await session.allocatedHost()
      return host === null
        ? envelope({
            consumerCount: 0,
            selectors: [],
            batches: [],
            disposed: false,
            afterCutoffLoss: { items: 0, bytes: 0 },
            disposeFailure: null
          })
        : host.continuationAccess.prepareClaim(maxItems, maxBytes)
    },
    async acknowledgeClaim(token) {
      const host = await session.allocatedHost()
      if (host === null) throw new Error('no process continuation owns this claim token')
      return host.continuationAccess.acknowledgeClaim(token)
    }
  })
}

module.exports = { createProcessControls }
