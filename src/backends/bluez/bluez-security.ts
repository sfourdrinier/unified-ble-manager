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

/** BlueZ system-mediated pairing only; Agent1/custom ceremonies are intentionally unsupported. */
export class BluezSecurityBackend implements SecurityBackend {
  private readonly streams = new Map<string, Set<CoreBoundedStream<PeerSecurityEvent>>>()
  private readonly activePairings = new Map<string, ActivePairing>()
  private readonly sequenceByPeer = new Map<string, number>()

  constructor(private readonly runtime: BluezBackendRuntime) {}

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
    if (this.activePairings.has(peerId)) {
      return Promise.reject(contractError('ownership.denied', 'platform', 'bluez.security.pair.arbitration'))
    }
    const path = this.runtime.devicePathForPeer(peerId)
    const current = this.readState(peerId)
    if (current.bond === 'bonded') return Promise.resolve({ outcome: 'already-paired', state: current })
    const controller = new AbortController()
    let pairCallStarted = false
    let pairCallSettled = false
    const dispatch = this.runtime.dispatcher.dispatch<SecurityPairResult>(
      { signal: options.signal, deadline: options.deadline },
      'bluez.security.pair',
      async () => {
        if (controller.signal.aborted) {
          throw contractError('operation.aborted', 'core', 'bluez.security.pair')
        }
        if (options.deadline !== null && options.deadline <= this.runtime.now()) {
          throw contractError('operation.timed-out', 'core', 'bluez.security.pair')
        }
        pairCallStarted = true
        try {
          await this.runtime.boundary.methods.callVoid(path, BLUEZ_DEVICE_INTERFACE, 'Pair', [])
        } finally {
          pairCallSettled = true
        }
        await waitForBluezBoolean(this.runtime, path, BLUEZ_DEVICE_INTERFACE, 'Paired', true, {
          signal: controller.signal,
          deadline: options.deadline
        })
        return { outcome: 'paired', state: this.readState(peerId) }
      },
      async () => {
        controller.abort()
        if (pairCallStarted && !pairCallSettled) {
          await this.cancelNativePairing(path)
        }
      }
    )
    const operation = dispatch.completion.catch(error => {
      if (
        error instanceof BackendContractError &&
        (error.normalized.code === 'operation.aborted' || error.normalized.code === 'operation.timed-out')
      ) {
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

  async cancelPairing(peerId: string, _options: PublicOperationOptions): Promise<SecurityCancelPairingResult> {
    this.runtime.assertUsable('bluez.security.cancel-pairing')
    const active = this.activePairings.get(peerId)
    if (active === undefined) return { outcome: 'not-pairing' }
    await active.dispatch.requestCancellation()
    return { outcome: 'cancelled' }
  }

  async unpair(peerId: string, _options: PublicOperationOptions): Promise<SecurityUnpairResult> {
    this.runtime.assertUsable('bluez.security.unpair')
    const dispatch = this.runtime.dispatcher.dispatch<SecurityUnpairResult>(
      _options,
      'bluez.security.unpair',
      async () => {
        const current = this.readState(peerId)
        if (current.bond !== 'bonded') return { outcome: 'already-unpaired' }
        const path = this.runtime.devicePathForPeer(peerId)
        const adapterPath = String(this.runtime.selectedAdapter.adapterId)
        await this.runtime.boundary.methods.callVoid(adapterPath, BLUEZ_ADAPTER_INTERFACE, 'RemoveDevice', [
          { signature: 'o', value: path }
        ])
        this.runtime.removePeerPath(peerId)
        return { outcome: 'unpaired' }
      }
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
