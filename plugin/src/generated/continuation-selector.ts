// Generated from src/backend-contract/continuation-selector.ts; do not edit.
function contractError(code: string, domain: string, detail: string): Error {
  return new Error(`${code}: ${domain}: ${detail}`)
}

/** One generation-resolved GATT characteristic in a native standing order. */
export interface BackgroundContinuationResubscribeSelector {
  readonly serviceUuid: string
  readonly serviceOccurrence: number
  readonly characteristicUuid: string
  readonly characteristicOccurrence: number
}

const KEYS = ['serviceUuid', 'serviceOccurrence', 'characteristicUuid', 'characteristicOccurrence']
const UUID = /^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$/

export function normalizeContinuationSelector(
  value: unknown,
  label = 'background.continuation.resubscribe'
): BackgroundContinuationResubscribeSelector {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) {
    throw contractError('argument.invalid', 'restoration', `${label}.entry`)
  }
  if (Object.keys(value).some(key => !KEYS.includes(key))) {
    throw contractError('argument.invalid', 'restoration', `${label}.entry.unknown-key`)
  }
  const uuid = (input: unknown, field: string) => {
    if (typeof input !== 'string' || !UUID.test(input)) {
      throw contractError('argument.invalid', 'restoration', `${label}.${field}`)
    }
    return input.toLowerCase()
  }
  const occurrence = (input: unknown, field: string) => {
    if (input === undefined) return 1
    if (typeof input !== 'number' || !Number.isSafeInteger(input) || input < 1) {
      throw contractError('argument.invalid', 'restoration', `${label}.${field}`)
    }
    return input
  }
  return Object.freeze({
    serviceUuid: uuid('serviceUuid' in value ? value.serviceUuid : undefined, 'serviceUuid'),
    serviceOccurrence: occurrence(
      'serviceOccurrence' in value ? value.serviceOccurrence : undefined,
      'serviceOccurrence'
    ),
    characteristicUuid: uuid(
      'characteristicUuid' in value ? value.characteristicUuid : undefined,
      'characteristicUuid'
    ),
    characteristicOccurrence: occurrence(
      'characteristicOccurrence' in value ? value.characteristicOccurrence : undefined,
      'characteristicOccurrence'
    )
  })
}
