// src/backends/legacy-native-require.ts
//
// PR210-28: the legacy node-gyp addon loaders use CommonJS `require`, which
// the ESM build does not define. Under ESM they fail with this typed
// `esm-legacy-boundary` cause instead of a masked "artifact unavailable".

import { contractError } from '../backend-contract/errors'

export function assertLegacyRequireAvailable(operation: string, domain: string): void {
  if (typeof require !== 'function') {
    throw contractError('capability.unsupported', 'platform', operation, {
      domain,
      code: 'esm-legacy-boundary',
      safeMessage: 'The legacy native boundary loader is CommonJS-only; load this module through the CommonJS build',
      metadata: Object.freeze({})
    })
  }
}
