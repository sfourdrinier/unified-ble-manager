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
  if (!isConnectionParameterValues(measured)) {
    throw contractError('protocol.violation', 'connection', operation)
  }
}

/** A protocol decoder can reject malformed measurements without throwing from its guard. */
export function isConnectionParameterValues(measured: {
  readonly intervalUs?: unknown
  readonly latency?: unknown
  readonly supervisionTimeoutUs?: unknown
}): measured is { readonly intervalUs: number; readonly latency: number; readonly supervisionTimeoutUs: number } {
  return (
    typeof measured.intervalUs === 'number' &&
    Number.isSafeInteger(measured.intervalUs) &&
    measured.intervalUs > 0 &&
    typeof measured.latency === 'number' &&
    Number.isSafeInteger(measured.latency) &&
    measured.latency >= 0 &&
    typeof measured.supervisionTimeoutUs === 'number' &&
    Number.isSafeInteger(measured.supervisionTimeoutUs) &&
    measured.supervisionTimeoutUs > 0
  )
}
