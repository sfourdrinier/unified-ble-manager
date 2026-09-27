import { isJsonObject, type JsonObject, type JsonValue } from './shared.ts'

export interface AccPoint {
  readonly x: number
  readonly y: number
  readonly z: number
}

function point(value: JsonValue | undefined): AccPoint | null {
  if (value === undefined || !isJsonObject(value)) return null
  const { x, y, z } = value
  return typeof x === 'number' &&
    Number.isFinite(x) &&
    typeof y === 'number' &&
    Number.isFinite(y) &&
    typeof z === 'number' &&
    Number.isFinite(z)
    ? { x, y, z }
    : null
}

function finite(value: JsonValue | undefined): number | null {
  return typeof value === 'number' && Number.isFinite(value) ? value : null
}

export function accTileView(record: JsonObject) {
  const points: AccPoint[] = []
  let error = record.accError === undefined || record.accError === null ? null : JSON.stringify(record.accError)
  if (Array.isArray(record.accDisplay)) {
    for (const value of record.accDisplay) {
      const parsed = point(value)
      if (parsed === null) {
        error = `${error === null ? '' : `${error}; `}Malformed ACC display sample`
        points.length = 0
        break
      }
      points.push(parsed)
    }
  } else if (record.accDisplay !== undefined) {
    error = `${error === null ? '' : `${error}; `}Malformed ACC display`
  }
  const settings = record.accSettings
  const settingsRecord = settings !== undefined && isJsonObject(settings) ? settings : {}
  return {
    points,
    last: point(record.lastAccMilliG),
    samples: finite(record.accSamples),
    rangeG: finite(settingsRecord.rangeG),
    sampleRateHz: finite(settingsRecord.sampleRateHz),
    error,
    loss: `${finite(record.pmdDroppedItems) ?? 0} dropped items · ${finite(record.pmdDroppedBytes) ?? 0} dropped bytes · ${finite(record.pmdReplacedItems) ?? 0} replaced items`
  }
}
