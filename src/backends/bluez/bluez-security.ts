import { BackendContractError, contractError, type CleanupRecord } from '../../backend-contract/errors'
import type { PublicOperationOptions } from '../../backend-contract/operations'
import type {
  PeerSecurityEvent,
  PeerSecurityState,
  SecurityBackend,
  SecurityCancelPairingResult,
  SecurityPairOptions,
  SecurityPairResult,
  SecurityUnpairResult
} from '../../backend-contract/security'
import { cancelOutcomeForPairResult } from '../../backend-contract/security'
import { capacity } from '../../backend-contract/primitives'
import { CoreBoundedStream } from '../../core/bounded-stream'
import type { BoundedAsyncStream, BoundedAsyncStreamIterator } from '../../backend-contract/streams'
import {
  BLUEZ_ADAPTER_INTERFACE,
  BLUEZ_DEVICE_INTERFACE,
  BluezDbusMethodError,
  type BluezPropertiesChanged
} from './bluez-dbus-contract'
import type { BluezBackendRuntime } from './bluez-backend-runtime'
import { waitForBluezBoolean } from './bluez-property-waiters'
import type { BluezOperationDispatch } from './bluez-operation-dispatcher'
import { normalizeBluezFailure } from './bluez-operation-dispatcher'
import { generationForSecureConnections, withPairingGeneration } from './bluez-pairing-generation'

const securityStreamLimits = Object.freeze({
  itemCapacity: capacity(16),
  byteCapacity: capacity(16 * 1024),
  reservedControlCapacity: capacity(1)
})

const securityLimitations = Object.freeze([
  Object.freeze({
    code: 'bluez-link-security-measurement-unavailable',
    explanation:
      'BlueZ Device1 exposes durable Paired state, but this backend does not infer current encryption or authentication from connection properties.',
    affectedGuarantee: 'encryption, authentication, and Secure Connections measurement'
  })
])

interface ActivePairing {
  readonly dispatch: BluezOperationDispatch<SecurityPairResult>
  readonly operation: Promise<SecurityPairResult>
}

class BluezSecurityStream implements BoundedAsyncStream<PeerSecurityEvent> {
  constructor(
    private readonly source: CoreBoundedStream<PeerSecurityEvent>,
    private readonly release: () => void
  ) {}

  get limits() {
    return this.source.limits
  }

  get overflowPolicy() {
    return this.source.overflowPolicy
  }

  [Symbol.asyncIterator](): BoundedAsyncStreamIterator<PeerSecurityEvent> {
    const sourceIterator = this.source[Symbol.asyncIterator]()
    const iterator: BoundedAsyncStreamIterator<PeerSecurityEvent> = {
      next: () => sourceIterator.next(),
      return: () => sourceIterator.return(),
      [Symbol.asyncIterator]: () => iterator
    }
    return iterator
  }

  async close(): Promise<CleanupRecord> {
    try {
      return await this.source.close()
    } finally {
      this.release()
    }
  }
}

export interface SecurityStreamOwnershipSnapshot {
  readonly peerCount: number
  readonly streamCount: number
}

const bluezSecurityOwnershipInspectors = new WeakMap<BluezSecurityBackend, () => SecurityStreamOwnershipSnapshot>()

export function inspectBluezSecurityStreamOwnershipForTests(
  backend: BluezSecurityBackend
): SecurityStreamOwnershipSnapshot {
  const inspect = bluezSecurityOwnershipInspectors.get(backend)
  if (inspect === undefined) {
    throw new Error('bluez security ownership inspector is missing')
  }
  return inspect()
}

function securityStreamOwnershipSnapshot(
  streams: ReadonlyMap<string, ReadonlySet<unknown>>
): SecurityStreamOwnershipSnapshot {
  let streamCount = 0
  for (const peerStreams of streams.values()) {
    streamCount += peerStreams.size
  }
  return { peerCount: streams.size, streamCount }
}

/** BlueZ system-mediated pairing only; a just-works Agent1 is registered by the boundary. Custom ceremonies are unsupported. */
/**
 * Whether a rejected `Device1.CancelPairing` still proves that no pairing is
 * left running.
 *
 * These two names answer the question rather than fail to answer it: BlueZ says
 * there is no bonding to cancel, or the device object is gone and can hold no
 * bonding at all. Either way nothing is in flight, so the cancellation got what
 * it asked for.
 *
 * Every other rejection - `org.bluez.Error.Failed`, `NotAuthorized`, a D-Bus
 * timeout, the daemon gone - means bluetoothd did not confirm that it stopped
 * bonding, so the in-flight `Pair` may still create one. Swallowing those was
 * reporting a pairing we could not stop as `cancelled`, which is the same
 * defect the Android backend was fixed for: a caller told no bond exists while
 * one is still being made cannot recover, because it never learns to look.
 *
 * The Tauri disconnect path classifies the identical pair of names for the
 * identical reason - a removed device object is an answer, not an error.
 */
function cancelProvesPairingAlreadyTerminal(error: BluezDbusMethodError): boolean {
  return (
    error.detail.name === 'org.bluez.Error.DoesNotExist' ||
    error.detail.name === 'org.freedesktop.DBus.Error.UnknownObject'
  )
}

export class BluezSecurityBackend implements SecurityBackend {
  private readonly streams = new Map<string, Set<CoreBoundedStream<PeerSecurityEvent>>>()
  private readonly activePairings = new Map<string, ActivePairing>()
  private readonly sequenceByPeer = new Map<string, number>()

  constructor(private readonly runtime: BluezBackendRuntime) {
    bluezSecurityOwnershipInspectors.set(this, () => securityStreamOwnershipSnapshot(this.streams))
  }

  state(peerId: string, _options: PublicOperationOptions): Promise<PeerSecurityState> {
    this.runtime.assertUsable('bluez.security.state')
    return Promise.resolve(this.readState(peerId))
  }

  watch(peerId: string): BoundedAsyncStream<PeerSecurityEvent> {
    this.runtime.assertUsable('bluez.security.watch')
    const source = new CoreBoundedStream<PeerSecurityEvent>(securityStreamLimits, 'error')
    const peerStreams = this.streams.get(peerId) ?? new Set<CoreBoundedStream<PeerSecurityEvent>>()
    peerStreams.add(source)
    this.streams.set(peerId, peerStreams)
    try {
      source.emit(this.createEvent(peerId, this.readState(peerId)), 1)
    } catch (error) {
      this.releaseStream(peerId, source)
      throw error
    }
    return new BluezSecurityStream(source, () => this.releaseStream(peerId, source))
  }

  pair(peerId: string, options: SecurityPairOptions): Promise<SecurityPairResult> {
    this.runtime.assertUsable('bluez.security.pair')
    if (options.ceremony !== 'system') {
      return Promise.reject(
        contractError('capability.unsupported', 'capability', 'bluez.security.pair.custom-ceremony')
      )
    }
    if (options.protection !== 'system-default') {
      return Promise.reject(contractError('capability.unsupported', 'capability', 'bluez.security.pair.protection'))
    }
    const directedGeneration =
      options.secureConnections !== undefined && options.secureConnections !== 'prefer'
        ? options.secureConnections
        : null
    const generationController = this.runtime.pairingGeneration
    if (directedGeneration !== null && generationController === null) {
      // BlueZ selects the pairing generation at the adapter level (mgmt
      // SET_SECURE_CONN), not per Device1.Pair; the D-Bus surface this backend
      // uses can neither force LE Legacy ('disallow') nor guarantee Secure
      // Connections ('require') for a single pairing. Without a host-supplied
      // privileged operation, honour the contract by failing closed rather than
      // bonding without the requested generation.
      return Promise.reject(
        contractError('capability.unsupported', 'capability', 'bluez.security.pair.secure-connections')
      )
    }
    if (this.activePairings.has(peerId)) {
      return Promise.reject(contractError('ownership.denied', 'platform', 'bluez.security.pair.arbitration'))
    }
    /**
     * Holds the adapter at the requested generation around the pairing, or runs
     * it untouched. `'prefer'` means defer to the platform, so it never reaches
     * here - only a directed `'require'`/`'disallow'` does, and only when the
     * host supplied the privileged operation.
     */
    const holdGeneration: <Result>(pair: () => Promise<Result>) => Promise<Result> =
      directedGeneration === null || generationController === null
        ? pair => pair()
        : pair =>
            withPairingGeneration(
              generationController,
              String(this.runtime.selectedAdapter.adapterId),
              generationForSecureConnections(directedGeneration),
              pair,
              failure => {
                // Reported, never swallowed: the adapter is left in the wrong
                // generation, which weakens every later bond on this host until
                // something sets it back.
                console.error(
                  `[unified-ble:bluez.security.pair] adapter ${failure.adapterId} left at ` +
                    `'${failure.heldGeneration}' because restoring '${failure.intendedGeneration}' failed: ` +
                    failure.detail
                )
              }
            )
    const path = this.runtime.devicePathForPeer(peerId)
    const current = this.readState(peerId)
    if (current.bond === 'bonded') return Promise.resolve({ outcome: 'already-paired', state: current })
    const controller = new AbortController()
    let pairCallStarted = false
    let pairCallSettled = false
    let pairSucceeded = false
    const dispatch = this.runtime.trackConnectionOperationForPeer(
      peerId,
      this.runtime.dispatcher.dispatch<SecurityPairResult>(
        { signal: options.signal, deadline: options.deadline },
        'bluez.security.pair',
        () =>
          holdGeneration(async () => {
            if (controller.signal.aborted) {
              throw contractError('operation.aborted', 'core', 'bluez.security.pair')
            }
            if (options.deadline !== null && options.deadline <= this.runtime.now()) {
              throw contractError('operation.timed-out', 'core', 'bluez.security.pair')
            }
            // BlueZ needs a registered Agent1 to drive even a just-works pairing.
            await this.runtime.boundary.ensurePairingAgent()
            // Registration is a real IPC round-trip; an abort or deadline that
            // lands while it is in flight runs onCancellation before Pair starts,
            // and it skips cancelNativePairing because pairCallStarted is still
            // false. Re-check here so we never fire Pair() for an operation that
            // was already cancelled - otherwise the caller sees 'cancelled' while
            // bluetoothd goes on to bond with no CancelPairing ever issued.
            if (controller.signal.aborted) {
              throw contractError('operation.aborted', 'core', 'bluez.security.pair')
            }
            if (options.deadline !== null && options.deadline <= this.runtime.now()) {
              throw contractError('operation.timed-out', 'core', 'bluez.security.pair')
            }
            try {
              // Mark started only immediately before Pair, so an abort during
              // agent registration does not cancel a pairing that never began.
              pairCallStarted = true
              await this.runtime.boundary.methods.callVoid(path, BLUEZ_DEVICE_INTERFACE, 'Pair', [])
              // Device1.Pair resolves only on a completed bond, so the peer is now
              // bonded regardless of whether the confirming Paired signal has
              // landed. Record that so a late abort is reported as paired, not
              // cancelled (which would strand the bond we just created).
              pairSucceeded = true
            } finally {
              pairCallSettled = true
            }
            await waitForBluezBoolean(this.runtime, path, BLUEZ_DEVICE_INTERFACE, 'Paired', true, {
              signal: controller.signal,
              deadline: options.deadline
            })
            return { outcome: 'paired', state: this.readState(peerId) }
          }),
        async () => {
          controller.abort()
          if (pairCallStarted && !pairCallSettled) {
            await this.cancelNativePairing(path)
          }
        }
      ),
      'bluez.security.pair'
    )
    const operation = dispatch.completion.catch(async error => {
      if (
        error instanceof BackendContractError &&
        (error.normalized.code === 'operation.aborted' || error.normalized.code === 'operation.timed-out')
      ) {
        // An abort or deadline that lands after Device1.Pair has already
        // completed cannot un-bond the peer, and onCancellation has correctly
        // skipped CancelPairing (pairCallSettled). Report the truth - a bond we
        // created - rather than 'cancelled', which would leave the caller
        // believing no pairing happened while a bond persists.
        if (pairSucceeded) {
          return { outcome: 'paired' as const, state: this.createState('bonded') }
        }
        return { outcome: 'cancelled' as const }
      }
      throw error
    })
    this.activePairings.set(peerId, { dispatch, operation })
    const settle = () => {
      const active = this.activePairings.get(peerId)
      if (active?.dispatch === dispatch) {
        this.activePairings.delete(peerId)
      }
    }
    dispatch.physicalSettlement.then(settle, settle).catch(() => undefined)
    return operation
  }

  async cancelPairing(peerId: string, options: PublicOperationOptions): Promise<SecurityCancelPairingResult> {
    this.runtime.assertUsable('bluez.security.cancel-pairing')
    const active = this.activePairings.get(peerId)
    // Narrow known window: the pairing can settle between this lookup and the
    // caller's call, so a cancellation that arrives at that instant reports
    // 'not-pairing'. Documented rather than closed - see docs/BONDING.md.
    if (active === undefined) return { outcome: 'not-pairing' }
    // Bound the whole cancellation transaction (CancelPairing acknowledgement
    // AND the pairing's own result). A timed-out wait is not a successful
    // physical cancellation, and giving up the wait does not drop the in-flight
    // pairing.
    const dispatch = this.runtime.dispatcher.dispatch(options, 'bluez.security.cancel-pairing', async () => {
      await active.dispatch.requestCancellation()
      return cancelOutcomeForPairResult(await active.operation)
    })
    return dispatch.completion
  }

  async unpair(peerId: string, _options: PublicOperationOptions): Promise<SecurityUnpairResult> {
    this.runtime.assertUsable('bluez.security.unpair')
    const dispatch = this.runtime.trackConnectionOperationForPeer(
      peerId,
      this.runtime.dispatcher.dispatch<SecurityUnpairResult>(_options, 'bluez.security.unpair', async () => {
        const current = this.readState(peerId)
        if (current.bond !== 'bonded') return { outcome: 'already-unpaired' }
        const path = this.runtime.devicePathForPeer(peerId)
        const adapterPath = String(this.runtime.selectedAdapter.adapterId)
        await this.runtime.boundary.methods.callVoid(adapterPath, BLUEZ_ADAPTER_INTERFACE, 'RemoveDevice', [
          { signature: 'o', value: path }
        ])
        this.runtime.removePeerPath(peerId)
        return { outcome: 'unpaired' }
      }),
      'bluez.security.unpair'
    )
    return dispatch.completion
  }

  propertiesChanged(event: BluezPropertiesChanged): void {
    if (event.interfaceName !== BLUEZ_DEVICE_INTERFACE || event.changed.Paired?.signature !== 'b') return
    const peerId = this.runtime.peerIdForPathIfKnown(event.path)
    if (peerId === null) return
    this.emit(peerId, this.readState(peerId))
  }

  peerRemoved(path: string): void {
    const peerId = this.runtime.peerIdForPathIfKnown(path)
    if (peerId === null) return
    this.activePairings
      .get(peerId)
      ?.dispatch.requestCancellation()
      .catch(() => undefined)
    const streams = this.streams.get(peerId)
    for (const stream of streams ?? []) stream.closeWithReason('operation-aborted')
    this.streams.delete(peerId)
  }

  reset(): void {
    for (const active of this.activePairings.values()) {
      active.dispatch.requestCancellation().catch(() => undefined)
    }
    for (const streams of this.streams.values()) {
      for (const stream of streams) stream.closeWithReason('source-failed')
    }
    this.streams.clear()
  }

  close(): void {
    for (const active of this.activePairings.values()) {
      active.dispatch.requestCancellation().catch(() => undefined)
    }
    for (const streams of this.streams.values()) {
      for (const stream of streams) stream.closeWithReason('owner-released')
    }
    this.streams.clear()
  }

  private async cancelNativePairing(path: string): Promise<void> {
    try {
      await this.runtime.boundary.methods.callVoid(path, BLUEZ_DEVICE_INTERFACE, 'CancelPairing', [])
    } catch (error) {
      if (!(error instanceof BluezDbusMethodError)) throw error
      if (!cancelProvesPairingAlreadyTerminal(error)) {
        throw normalizeBluezFailure(error, 'bluez.security.cancel-pairing')
      }
    }
  }

  private readState(peerId: string): PeerSecurityState {
    const path = this.runtime.devicePathForPeer(peerId)
    const paired = this.runtime.store.optionalBooleanProperty(path, BLUEZ_DEVICE_INTERFACE, 'Paired')
    const state = this.createState(paired === true ? 'bonded' : paired === false ? 'not-bonded' : 'unknown')
    return state
  }

  private createState(bond: PeerSecurityState['bond']): PeerSecurityState {
    return Object.freeze({
      bond,
      encryption: 'unsupported',
      authentication: 'unsupported',
      secureConnections: 'unsupported',
      pairingPossible: true,
      measuredAtMonotonicMs: this.runtime.now(),
      limitations: securityLimitations
    })
  }

  private emit(peerId: string, state: PeerSecurityState): void {
    const streams = this.streams.get(peerId)
    if (streams === undefined) return
    const event = this.createEvent(peerId, state)
    for (const stream of [...streams]) {
      if (stream.emit(event, 1).terminated) streams.delete(stream)
    }
    if (streams.size === 0) this.streams.delete(peerId)
  }

  private releaseStream(peerId: string, stream: CoreBoundedStream<PeerSecurityEvent>): void {
    const streams = this.streams.get(peerId)
    if (streams === undefined) return
    streams.delete(stream)
    if (streams.size === 0) this.streams.delete(peerId)
  }

  private createEvent(peerId: string, state: PeerSecurityState): PeerSecurityEvent {
    const sequence = (this.sequenceByPeer.get(peerId) ?? 0) + 1
    this.sequenceByPeer.set(peerId, sequence)
    return Object.freeze({ kind: 'state', peerId, sequence, state })
  }
}
