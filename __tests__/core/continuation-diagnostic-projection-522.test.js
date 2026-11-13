const { openNativeContinuationRecordings } = require('../../src/backends/desktop/native-continuation-recording')

const base = {
  recordingId: 'h10',
  phase: 'stopped',
  accepting: false,
  records: 0,
  bytes: 0,
  lostRecords: 0,
  maxBytes: 1048576,
  maxRecords: 10,
  encrypted: false,
  runtimeFailure: null,
  collectionFailure: null
}
const failure = {
  kind: 'storage.io',
  detail: 'é'.repeat(2000),
  operation: 'append',
  sqliteExtendedCode: null,
  sqliteCode: null
}
const marker = source => ({
  kind: 'storage.diagnostic-oversized',
  detail: 'retained diagnostic exceeds status projection limit',
  operation: 'status',
  persisted: true,
  payloadBytes: 65537,
  source
})

function publicProjection(snapshot) {
  const runtimeDiagnostic =
    snapshot.runtimeFailure !== null && snapshot.runtimeFailure.persisted === true ? snapshot.runtimeFailure : undefined
  const collectionDiagnostic =
    snapshot.collectionFailure !== null &&
    ('retainedSummary' in snapshot.collectionFailure ||
      'pendingRecordedLoss' in snapshot.collectionFailure ||
      snapshot.collectionFailure.kind.startsWith('storage.diagnostic-'))
      ? snapshot.collectionFailure
      : undefined
  const legacyCollection =
    collectionDiagnostic === undefined || snapshot.collectionFailure === null
      ? snapshot.collectionFailure
      : snapshot.collectionFailure.kind.startsWith('storage.diagnostic-')
        ? null
        : (() => {
            const { retainedSummary, pendingRecordedLoss, ...legacy } = snapshot.collectionFailure
            return legacy
          })()
  return {
    ...snapshot,
    runtimeFailure: runtimeDiagnostic === undefined ? snapshot.runtimeFailure : null,
    ...(runtimeDiagnostic === undefined ? {} : { runtimeDiagnostic }),
    collectionFailure: legacyCollection,
    ...(collectionDiagnostic === undefined ? {} : { collectionDiagnostic })
  }
}

test.each([
  { runtimeFailure: { ...failure, persisted: true } },
  { runtimeFailure: marker('occurrence') },
  { runtimeFailure: { ...marker('occurrence'), kind: 'storage.diagnostic-unreadable', payloadBytes: 12 } },
  { collectionFailure: marker('collectionSummary') },
  { collectionFailure: { ...marker('collectionSummary'), kind: 'storage.diagnostic-unreadable', payloadBytes: 17 } },
  {
    collectionFailure: {
      ...failure,
      persisted: false,
      retainedSummary: { ...marker('collectionSummary'), kind: 'storage.diagnostic-unreadable', payloadBytes: 17 }
    }
  },
  {
    collectionFailure: {
      ...failure,
      persisted: false,
      persistenceFailure: failure,
      retainedSummary: marker('collectionSummary'),
      pendingRecordedLoss: { items: 2, bytes: 9, persisted: false }
    }
  },
  {
    collectionFailure: {
      ...failure,
      persisted: true,
      retainedSummary: marker('collectionSummary'),
      pendingRecordedLoss: { items: 2, bytes: 9, persisted: true }
    }
  }
])('native public status and stop accept Rust diagnostic projection %#', async projection => {
  const snapshot = { ...base, ...projection }
  const envelope = value => JSON.stringify({ ok: true, value })
  const store = {
    status: async () => envelope(snapshot),
    stop: async () => envelope({ ...snapshot, radioRelease: 'not-requested' })
  }
  const controller = await openNativeContinuationRecordings({ openRecordingStore: async () => store }, '/private/app')
  expect(await controller.status('h10')).toEqual(publicProjection(snapshot))
  expect(await controller.stop('h10')).toEqual({ ...publicProjection(snapshot), radioRelease: 'not-requested' })
})

test('public diagnostic projection rejects invalid markers and oversized field bytes', async () => {
  let runtimeFailure = { ...marker('occurrence'), source: 'other' }
  const store = { status: async () => JSON.stringify({ ok: true, value: { ...base, runtimeFailure } }) }
  const controller = await openNativeContinuationRecordings({ openRecordingStore: async () => store }, '/private/app')
  await expect(controller.status('h10')).rejects.toMatchObject({ code: 'protocol.malformed' })
  runtimeFailure = { ...failure, detail: 'é'.repeat(32769), persisted: true }
  await expect(controller.status('h10')).rejects.toMatchObject({ code: 'protocol.malformed' })
  runtimeFailure = { ...marker('occurrence'), kind: 'storage.diagnostic-unreadable', payloadBytes: -1 }
  await expect(controller.status('h10')).rejects.toMatchObject({ code: 'protocol.malformed' })
})

test('public status rejects inconsistent pending loss and a known historical total', async () => {
  let collectionFailure = {
    ...failure,
    persisted: true,
    retainedSummary: marker('collectionSummary'),
    pendingRecordedLoss: { items: 2, persisted: false }
  }
  const store = { status: async () => JSON.stringify({ ok: true, value: { ...base, collectionFailure } }) }
  const controller = await openNativeContinuationRecordings({ openRecordingStore: async () => store }, '/private/app')
  await expect(controller.status('h10')).rejects.toMatchObject({ code: 'protocol.malformed' })
  collectionFailure = {
    ...collectionFailure,
    pendingRecordedLoss: { items: 2, persisted: false },
    knownRecordedLoss: { items: 2 }
  }
  await expect(controller.status('h10')).rejects.toMatchObject({ code: 'protocol.malformed' })
})
