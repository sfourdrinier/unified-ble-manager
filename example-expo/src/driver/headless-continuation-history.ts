import type { JsonObject } from '../../../examples-shared/driver/protocol.ts'

/** One app-private authority shared by the task writer and explicit diagnostics. */
export const HEADLESS_EVIDENCE_KEY = 'ubm.reference.headless-continuation.v1'
const LIMIT = 16

function invalid(): never {
  throw new Error('Headless evidence storage has an invalid history')
}
function object(value: unknown): Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return invalid()
  return Object.fromEntries(Object.entries(value))
}
function errorIdentity(error: unknown): JsonObject {
  const value = object(error)
  const detail = typeof value.detail === 'object' && value.detail !== null && !Array.isArray(value.detail)
    ? object(value.detail) : {}
  const errorCode = identityToken(value.code)
  const errorDomain = identityToken(detail.domain)
  const errorOperation = identityToken(detail.operation)
  return {
    ...(errorCode === undefined ? {} : { errorCode }),
    ...(errorDomain === undefined ? {} : { errorDomain }),
    ...(errorOperation === undefined ? {} : { errorOperation }),
    ...([value.code, detail.domain, detail.operation].some(token => token !== undefined && identityToken(token) === undefined)
      ? { errorIdentityRedacted: true } : {})
  }
}
function identityToken(token: unknown) {
  return typeof token === 'string' && /^[A-Za-z][A-Za-z0-9._-]{0,127}$/.test(token) ? token : undefined
}
function summary(input: unknown): JsonObject {
  const value = object(input)
  if (
    Object.keys(value).some(
      key => !['state', 'peerId', 'observedAtMs', 'batteryPercent', 'cleanup', 'error', 'operation'].includes(key)
    )
  )
    return invalid()
  if (value.state === 'failed' && value.operation === 'payload') {
    if (Object.keys(value).length !== 3 || typeof object(value.error).message !== 'string') return invalid()
    return { state: 'failed', operation: 'payload', failed: true }
  }
  if (
    !['started', 'completed', 'failed'].includes(String(value.state)) ||
    typeof value.peerId !== 'string' ||
    !/^(?:[0-9a-f]{2}:){5}[0-9a-f]{2}$/i.test(value.peerId) ||
    typeof value.observedAtMs !== 'number' ||
    !Number.isFinite(value.observedAtMs) ||
    value.observedAtMs < 0 ||
    value.operation !== undefined
  )
    return invalid()
  if (value.state === 'started') {
    if (Object.keys(value).length !== 3) return invalid()
    return { state: 'started', observedAtMs: value.observedAtMs }
  }
  if (
    value.batteryPercent !== undefined &&
    (typeof value.batteryPercent !== 'number' ||
      !Number.isInteger(value.batteryPercent) ||
      value.batteryPercent < 0 ||
      value.batteryPercent > 100)
  )
    return invalid()
  const cleanup = object(value.cleanup)
  if (!['released', 'release-failed', 'rejected', 'not-acquired'].includes(String(cleanup.state))) return invalid()
  if (cleanup.state === 'released' || cleanup.state === 'release-failed') {
    if (
      !Array.isArray(cleanup.failures) ||
      (cleanup.state === 'released' && cleanup.failures.length !== 0) ||
      (cleanup.state === 'release-failed' && cleanup.failures.length === 0)
    )
      return invalid()
    for (const failure of cleanup.failures) {
      const record = object(failure)
      if (typeof record.resourceKind !== 'string' || typeof object(record.error).code !== 'string') return invalid()
    }
  }
  if (cleanup.state === 'rejected' && typeof object(cleanup.error).message !== 'string') return invalid()
  if (
    value.state === 'completed' &&
    (value.batteryPercent === undefined || cleanup.state !== 'released' || value.error !== undefined)
  )
    return invalid()
  if (value.state === 'failed' && typeof object(value.error).message !== 'string') return invalid()
  return {
    state: String(value.state),
    observedAtMs: value.observedAtMs,
    ...(typeof value.batteryPercent === 'number' ? { batteryPercent: value.batteryPercent } : {}),
    cleanupState: String(cleanup.state),
    cleanupFailures: Array.isArray(cleanup.failures) ? cleanup.failures.length : 0,
    failed: value.state === 'failed',
    ...(value.state === 'failed' ? errorIdentity(value.error) : {})
  }
}
function decode(text: string | null): unknown[] {
  if (text === null) return []
  let value: unknown
  try {
    value = JSON.parse(text)
  } catch {
    return invalid()
  }
  if (!Array.isArray(value) || value.length > LIMIT) return invalid()
  for (const entry of value) summary(entry)
  return value
}

export function createHeadlessHistory(storage: {
  getItem(key: string): Promise<string | null>
  setItem(key: string, value: string): Promise<void>
}) {
  return {
    async read(): Promise<JsonObject> {
      const records = decode(await storage.getItem(HEADLESS_EVIDENCE_KEY)).map(summary)
      return { records, count: records.length }
    },
    async save(evidence: JsonObject): Promise<void> {
      summary(evidence)
      const previous = decode(await storage.getItem(HEADLESS_EVIDENCE_KEY))
      await storage.setItem(HEADLESS_EVIDENCE_KEY, JSON.stringify([...previous.slice(-(LIMIT - 1)), evidence]))
    }
  }
}
