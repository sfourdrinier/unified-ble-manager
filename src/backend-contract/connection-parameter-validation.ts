import { contractError } from './errors'

/** Snapshot and event paths admit the same measured connection facts. */
export function assertConnectionParameterValues(
  measured: { readonly intervalUs?: unknown; readonly latency?: unknown; readonly supervisionTimeoutUs?: unknown },
  operation: string
): asserts measured is {
  readonly intervalUs: number
  readonly latency: number
  readonly supervisionTimeoutUs: number
} {
  if (
    typeof measured.intervalUs !== 'number' ||
    !Number.isFinite(measured.intervalUs) ||
    measured.intervalUs <= 0 ||
    typeof measured.latency !== 'number' ||
    !Number.isSafeInteger(measured.latency) ||
    measured.latency < 0 ||
    typeof measured.supervisionTimeoutUs !== 'number' ||
    !Number.isFinite(measured.supervisionTimeoutUs) ||
    measured.supervisionTimeoutUs <= 0
  ) {
    throw contractError('protocol.violation', 'connection', operation)
  }
}
