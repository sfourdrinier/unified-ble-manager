// src/backend-contract/recovery.ts — canonical error recovery authority

import type { BleErrorCode } from './errors'

export type BleRecoveryDisposition =
  | 'none'
  | 'retry-immediately'
  | 'retry-with-backoff'
  | 'after-state-change'
  | 'after-user-action'
  | 'caller-policy'

export type RecoveryAction =
  | { readonly kind: 'request-permission'; readonly permission: string }
  | { readonly kind: 'open-settings'; readonly target: 'app' | 'bluetooth' | 'location' }
  | { readonly kind: 'wait-for-adapter'; readonly desired: 'powered-on' | 'available' }
  | { readonly kind: 'rescan' }
  | { readonly kind: 'reselect-peer' }
  | { readonly kind: 'reconnect' }
  | { readonly kind: 'rediscover-gatt' }
  | { readonly kind: 'repair' }
  | { readonly kind: 'reduce-payload'; readonly maximumBytes: number | null }
  | { readonly kind: 'wait-for-write-ready' }
  | { readonly kind: 'recreate-manager' }
  | { readonly kind: 'retry'; readonly afterMs: number | null }

export interface BleRecovery {
  readonly disposition: BleRecoveryDisposition
  readonly actions: readonly RecoveryAction[]
}

export function recoveryForCode(code: BleErrorCode, operation: string): BleRecovery {
  switch (code) {
    case 'protocol.incompatible':
    case 'protocol.malformed':
    case 'protocol.violation':
    case 'lifecycle.invariant-violation':
    case 'argument.invalid':
    case 'bytes.invalid':
      return { disposition: 'none', actions: [] }
    case 'bytes.too-large':
      return { disposition: 'none', actions: [{ kind: 'reduce-payload', maximumBytes: null }] }
    case 'lifecycle.destroyed':
    case 'lifecycle.invalid-state':
    case 'backend.reset':
    case 'operation.cancelled-by-destroy':
      return { disposition: 'none', actions: [{ kind: 'recreate-manager' }] }
    case 'adapter.unavailable':
    case 'adapter.resetting':
    case 'adapter.ambiguous':
    case 'operation.adapter-unavailable':
      return {
        disposition: 'after-state-change',
        actions: [{ kind: 'wait-for-adapter', desired: 'available' }]
      }
    case 'adapter.powered-off':
      return {
        disposition: 'after-state-change',
        actions: [{ kind: 'wait-for-adapter', desired: 'powered-on' }]
      }
    case 'adapter.selection-required':
      return { disposition: 'after-user-action', actions: [{ kind: 'reselect-peer' }] }
    case 'permission.denied':
      return {
        disposition: 'after-user-action',
        actions: [
          { kind: 'request-permission', permission: operation },
          { kind: 'open-settings', target: 'app' }
        ]
      }
    case 'permission.restricted':
      return { disposition: 'after-user-action', actions: [{ kind: 'open-settings', target: 'app' }] }
    case 'permission.not-determined':
      return { disposition: 'after-user-action', actions: [{ kind: 'request-permission', permission: operation }] }
    case 'ownership.denied':
    case 'connection.already-owned':
    case 'scan.already-active':
    case 'chooser.busy':
      return { disposition: 'none', actions: [] }
    case 'scan.start-failed':
    case 'scan.stop-failed':
    case 'scan.filter-invalid':
      return { disposition: 'none', actions: [{ kind: 'rescan' }] }
    case 'chooser.cancelled':
    case 'chooser.closed':
    case 'chooser.user-activation-required':
    case 'chooser.insecure-context':
    case 'chooser.api-unavailable':
    case 'chooser.optional-service-not-granted':
    case 'chooser.permitted-device-unavailable':
      return { disposition: 'after-user-action', actions: [{ kind: 'reselect-peer' }] }
    case 'connection.not-found':
    case 'connection.failed':
    case 'connection.stale':
    case 'connection.lost':
    case 'operation.disconnected':
    case 'operation.reset':
      return { disposition: 'retry-with-backoff', actions: [{ kind: 'reconnect' }] }
    case 'gatt.discovery-required':
    case 'gatt.stale-handle':
    case 'gatt.cache-unknown':
      return { disposition: 'retry-immediately', actions: [{ kind: 'rediscover-gatt' }] }
    case 'gatt.ambiguous-path':
    case 'gatt.not-found':
    case 'gatt.property-not-supported':
    case 'gatt.read-failed':
    case 'gatt.write-failed':
    case 'gatt.subscribe-failed':
      return { disposition: 'none', actions: [] }
    case 'gatt.cccd-managed':
      return { disposition: 'none', actions: [{ kind: 'wait-for-write-ready' }] }
    case 'stream.overflow':
    case 'stream.closed':
    case 'stream.quota':
    case 'stream.rate-limited':
      return { disposition: 'retry-with-backoff', actions: [{ kind: 'retry', afterMs: null }] }
    case 'capability.unsupported':
    case 'capability.unavailable':
    case 'capability.limited':
      return { disposition: 'none', actions: [] }
    case 'background.terminated':
      return { disposition: 'after-state-change', actions: [{ kind: 'reconnect' }] }
    case 'platform.failure':
    case 'platform.security':
    case 'platform.transport':
      return { disposition: 'caller-policy', actions: [] }
    case 'operation.aborted':
    case 'operation.timed-out':
      return { disposition: 'caller-policy', actions: [{ kind: 'retry', afterMs: null }] }
  }
}
