import { contractError } from './backend-contract/errors'
import { toPublicCleanupRecord, type CleanupRecord } from './public/cleanup'
import { compensateDesktopInitialization } from './desktop-process-initialization'
export { DesktopProcessHostInitializationError } from './desktop-process-initialization'
import { byteLimit, opaqueId } from './backend-contract/primitives'
import type { HostNeutralBackendIdentity } from './backend-contract/identity'
import { normalizeBackgroundContinuation } from './backend-contract/background-continuation'
import { DesktopRustCoreBackend, DESKTOP_RUST_CORE_PROFILES } from './backends/desktop/desktop-rust-core-provider'
import type {
  NativeContinuationController,
  NativeContinuationControlAccess
} from './backends/desktop/native-continuation-controller'
import {
  attachBleBackend,
  createManagerOwnershipAuthority,
  createBleManager,
  DEFAULT_BLE_MANAGER_OPTIONS
} from './manager/ble-manager'
import type { BleManager as InternalBleManager } from './manager/ble-manager'
import { createEphemeralHostIdentity, normalizeBleManagerCreateOptions } from './public/host-identity'
import type { BleManagerCreateOptions } from './public/host-identity'
import { createPublicBleManager, type BleManager } from './public/ble-manager'
import { rehydratePublicPromise } from './public/error-bridge'

export type DesktopProcessManagerOptions = Pick<BleManagerCreateOptions, 'instanceId' | 'diagnostics'>
export type DesktopProcessInternalManager = InternalBleManager<string, HostNeutralBackendIdentity<string>>

/** Trusted process lifetime: child destruction never shuts down the shared radio. */
export interface DesktopProcessHost {
  readonly continuation: NativeContinuationController
  /** Trusted authenticated bridge only: prepare/deliver/ack are separate operations. */
  readonly continuationAccess: NativeContinuationControlAccess
  createManager(options?: DesktopProcessManagerOptions): Promise<BleManager>
  /** Trusted main-process adapter for routers using the existing advanced manager contract. */
  createInternalManager(options?: DesktopProcessManagerOptions): Promise<DesktopProcessInternalManager>
  /** Closes admission, revokes children, then releases the native owner. Retry failed receipts. Never acknowledges recordings. */
  destroy(): Promise<CleanupRecord>
}

/** Internal construction over one selected backend; also used by real-addon tests. */
export async function createDesktopProcessHostFromBackend(
  backend: DesktopRustCoreBackend,
  options: DesktopProcessManagerOptions & {
    readonly now: () => number
    readonly randomBytes?: (length: number) => Uint8Array
  }
): Promise<DesktopProcessHost> {
  try {
    return await initializeDesktopProcessHost(backend, options)
  } catch (originalCause) {
    return rehydratePublicPromise(compensateDesktopInitialization(originalCause, () => backend.destroy()))
  }
}

async function initializeDesktopProcessHost(
  backend: DesktopRustCoreBackend,
  options: DesktopProcessManagerOptions & {
    readonly now: () => number
    readonly randomBytes?: (length: number) => Uint8Array
  }
): Promise<DesktopProcessHost> {
  const profile = Object.values(DESKTOP_RUST_CORE_PROFILES).find(
    item => item.backendId === backend.identity.registeredBackendId
  )
  if (profile === undefined) throw contractError('protocol.incompatible', 'core', 'desktop-process-host.backend')
  const native = backend.nativeContinuationController()
  const nativeAccess = backend.nativeContinuationControlAccess()
  const attachedBackend = await attachBleBackend(backend, profile.compatibility)
  const authority = createManagerOwnershipAuthority(attachedBackend)
  const managerOptions = (input: DesktopProcessManagerOptions) => ({
    ...DEFAULT_BLE_MANAGER_OPTIONS,
    now: options.now,
    maximumValueBytes:
      input.diagnostics?.maximumValueBytes === undefined
        ? DEFAULT_BLE_MANAGER_OPTIONS.maximumValueBytes
        : byteLimit(input.diagnostics.maximumValueBytes),
    traceMaximumRecords: input.diagnostics?.traceMaximumRecords ?? DEFAULT_BLE_MANAGER_OPTIONS.traceMaximumRecords,
    traceMaximumBytes: input.diagnostics?.traceMaximumBytes ?? DEFAULT_BLE_MANAGER_OPTIONS.traceMaximumBytes
  })
  const identity = (instanceId?: string) => {
    const value = createEphemeralHostIdentity({ randomBytes: options.randomBytes })
    const suffix = instanceId === undefined ? '' : `-${instanceId}`
    return {
      clientId: opaqueId(`desktop-${value.managerNonce}${suffix}`, 'client', 'desktop:process-host'),
      managerId: opaqueId(`desktop-${value.attachmentNonce}${suffix}`, 'manager', 'desktop:process-host')
    }
  }
  const owner = await createBleManager(
    { attachedBackend, ...identity(options.instanceId), ownerMode: 'owning' },
    authority,
    managerOptions(options)
  )
  let closing = false
  let destruction: Promise<CleanupRecord> | null = null
  const pendingChildren = new Set<Promise<unknown>>()
  const admit = () => {
    if (closing) throw contractError('lifecycle.destroyed', 'core', 'desktop-process-host.closed')
  }
  const child = <T>(
    input: DesktopProcessManagerOptions,
    publish: (manager: DesktopProcessInternalManager) => Promise<T>
  ) =>
    rehydratePublicPromise(
      (async () => {
        admit()
        const normalized = normalizeBleManagerCreateOptions(input)
        if (Object.keys(input).some(key => key !== 'instanceId' && key !== 'diagnostics'))
          throw contractError('argument.invalid', 'core', 'desktop-process-host.child-options')
        const manager = await createBleManager(
          { attachedBackend, ...identity(normalized.instanceId), ownerMode: 'borrowing' },
          authority,
          managerOptions(normalized)
        )
        try {
          const result = await publish(manager)
          admit()
          return result
        } catch (error) {
          return compensateDesktopInitialization(error, () => manager.destroy())
        }
      })()
    )
  const trackChild = <T>(
    input: DesktopProcessManagerOptions,
    publish: (manager: DesktopProcessInternalManager) => Promise<T>
  ) => {
    const promise = child(input, publish)
    pendingChildren.add(promise)
    promise.then(
      () => pendingChildren.delete(promise),
      () => pendingChildren.delete(promise)
    )
    return promise
  }
  const continuation = Object.freeze<NativeContinuationController>({
    execute: declaration =>
      rehydratePublicPromise(
        (async () => {
          admit()
          const result = await native.execute(declaration)
          admit()
          return result
        })()
      ),
    recordings: directory =>
      rehydratePublicPromise(
        (async () => {
          admit()
          const result = await native.recordings(directory)
          admit()
          return result
        })()
      ),
    claim: input => native.claim(input),
    status: () => native.status()
  })
  return Object.freeze<DesktopProcessHost>({
    continuation,
    continuationAccess: Object.freeze<NativeContinuationControlAccess>({
      execute: (peerId, declarationJson) =>
        rehydratePublicPromise(
          (async () => {
            admit()
            const result = await nativeAccess.execute(peerId, declarationJson)
            admit()
            return result
          })()
        ),
      describeBacklog: () => nativeAccess.describeBacklog(),
      prepareClaim: (maxItems, maxBytes) => nativeAccess.prepareClaim(maxItems, maxBytes),
      acknowledgeClaim: token => nativeAccess.acknowledgeClaim(token)
    }),
    createInternalManager: (input = {}) => trackChild(input, async manager => manager),
    createManager: (input = {}) => trackChild(input, manager => createPublicBleManager(manager, options.now)),
    destroy: () => {
      closing = true
      if (destruction === null) {
        destruction = rehydratePublicPromise(
          (async () => {
            const receipt = await owner.destroy()
            await Promise.allSettled([...pendingChildren])
            return toPublicCleanupRecord(receipt)
          })()
        ).then(
          result => {
            if (result.state !== 'released') destruction = null
            return result
          },
          error => {
            destruction = null
            throw error
          }
        )
      }
      return destruction
    }
  })
}

/** One-call factory options remain explicit; continuation is driven through the returned host. */
export function assertDesktopProcessOptions(options: BleManagerCreateOptions): void {
  const normalized = normalizeBleManagerCreateOptions(options)
  if (
    normalized.restoration !== undefined ||
    normalizeBackgroundContinuation(normalized.background?.continuation).onAppearance !== 'record-only'
  ) {
    throw contractError('capability.unsupported', 'restoration', 'desktop-process-host.use-explicit-continuation')
  }
}
