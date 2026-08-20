// __tests__/docs-recipes/finite-hrs-journey.js

const {
  defaultScanDelivery,
  firstNotification,
  scanUntil,
  throwIfCleanupFailed,
  withConnection
} = require('../../src')
const { resolveCharacteristicPath } = require('../../src/profiles/commands')
const {
  HEART_RATE_SERVICE,
  heartRateMeasurementSelector,
  parseHeartRateMeasurement
} = require('../../src/profiles/heart-rate')

async function runFiniteHrsJourney({ manager, settle, onPeer, onNotify }) {
  const abort = new AbortController()
  const journeyDeadline = null
  const op = { signal: abort.signal, deadline: journeyDeadline }
  let measurement = null
  try {
    const foundPromise = scanUntil(manager, {
      scan: {
        filter: {
          serviceUuids: [HEART_RATE_SERVICE],
          manufacturerData: [],
          localNamePrefix: null
        },
        duplicatePolicy: 'merged',
        timestampPolicy: 'source-then-receipt',
        delivery: defaultScanDelivery(),
        deadline: journeyDeadline,
        signal: abort.signal,
        sharing: { mode: 'owner', allowSharing: false }
      },
      matches: candidate => candidate.localName.state === 'present'
    })
    await onPeer()
    const observation = await settle(foundPromise)
    await settle(
      withConnection(manager, observation.device.id, op, async connection => {
        const database = await connection.discover(op)
        const snapshot = await database.snapshot()
        const measurementPath = await resolveCharacteristicPath(snapshot, heartRateMeasurementSelector())
        const pending = firstNotification(database, measurementPath, {
          ...op,
          delivery: defaultScanDelivery()
        })
        await onNotify(measurementPath)
        const bytes = await settle(pending)
        measurement = parseHeartRateMeasurement(bytes)
      })
    )
  } finally {
    throwIfCleanupFailed(await settle(manager.destroy()), 'manager.destroy')
  }
  return measurement
}

module.exports = { runFiniteHrsJourney }
