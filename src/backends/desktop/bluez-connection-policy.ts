import { contractError } from '../../backend-contract/errors'

/** LE-only BlueZ authority. The native host resolves and pins the daemon by default. */
export interface BluezConnectionPolicy {
  readonly mode: 'le-bearer'
  /** Optional stricter host pin; a mismatch is refused rather than rebound. */
  readonly daemonUniqueOwner?: string
}

/** Validate before allocation. Omission delegates owner resolution to native authority. */
export function admitBluezConnectionPolicy(value: unknown): BluezConnectionPolicy | undefined {
  if (value === undefined) return undefined
  if (
    value === null ||
    typeof value !== 'object' ||
    Array.isArray(value) ||
    !('mode' in value) ||
    value.mode !== 'le-bearer' ||
    !Object.keys(value).every(key => key === 'mode' || key === 'daemonUniqueOwner')
  ) {
    throw contractError('argument.invalid', 'core', 'bluez.connection-policy')
  }
  if (!('daemonUniqueOwner' in value)) return undefined
  if (
    typeof value.daemonUniqueOwner === 'string' &&
    value.daemonUniqueOwner.length <= 255 &&
    /^:[A-Za-z0-9_-]+(?:\.[A-Za-z0-9_-]+)+$/u.test(value.daemonUniqueOwner) &&
    !/\s/u.test(value.daemonUniqueOwner)
  ) {
    return Object.freeze({ mode: 'le-bearer', daemonUniqueOwner: value.daemonUniqueOwner })
  }
  throw contractError('argument.invalid', 'core', 'bluez.connection-policy')
}
