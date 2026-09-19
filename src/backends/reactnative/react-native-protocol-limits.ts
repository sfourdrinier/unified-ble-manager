// src/backends/reactnative/react-native-protocol-limits.ts
//
// React Native host limits and restoration outcomes the shipped route
// reports. They first came from the legacy Native Protocol v2 schema; a
// guard test pins them to it for as long as that schema still exists.

/** Largest restoration control record, in bytes. */
export const MAXIMUM_CONTROL_RECORD_BYTES = 262144

/** Largest binary payload (descriptor value) one operation carries, in bytes. */
export const MAXIMUM_BINARY_PAYLOAD_BYTES = 524288

export const restorationOutcomes = Object.freeze([
  'adopted',
  'alreadyConsumed',
  'attachmentMismatch',
  'backendMismatch',
  'namespaceMismatch',
  'epochMismatch'
])

export type RestorationOutcomes = (typeof restorationOutcomes)[number]
