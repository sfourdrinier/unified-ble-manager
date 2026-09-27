import type { JsonObject } from './shared.ts'

/** Retain the scenario's structured cause, not just its coarse tile status. */
export function tileFailureView(record: JsonObject): string | null {
  return record.error === undefined || record.error === null ? null : JSON.stringify(record.error)
}
