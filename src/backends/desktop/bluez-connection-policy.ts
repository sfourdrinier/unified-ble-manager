import { contractError } from '../../backend-contract/errors'

/** Trusted host attestation of an implemented LE-only BlueZ bearer API. */
export interface BluezConnectionPolicy {
  readonly mode: 'le-bearer'
  /** Current unique D-Bus owner of org.bluez; a well-known name is not an owner pin. */
  readonly daemonUniqueOwner: string
}

/** Validate and snapshot policy before any backend allocation. Omission is scan-only. */
export function admitBluezConnectionPolicy(value: unknown): BluezConnectionPolicy | undefined {
  if (value === undefined) return undefined
  if (
    value !== null &&
    typeof value === 'object' &&
    !Array.isArray(value) &&
    'mode' in value &&
    value.mode === 'le-bearer' &&
    'daemonUniqueOwner' in value &&
    typeof value.daemonUniqueOwner === 'string' &&
    value.daemonUniqueOwner.length <= 255 &&
    /^:[A-Za-z0-9_-]+(?:\.[A-Za-z0-9_-]+)+$/u.test(value.daemonUniqueOwner) &&
    !/\s/u.test(value.daemonUniqueOwner) &&
    Object.keys(value).every(key => key === 'mode' || key === 'daemonUniqueOwner')
  ) {
    return Object.freeze({ mode: 'le-bearer', daemonUniqueOwner: value.daemonUniqueOwner })
  }
  throw contractError('argument.invalid', 'core', 'bluez.connection-policy')
}
