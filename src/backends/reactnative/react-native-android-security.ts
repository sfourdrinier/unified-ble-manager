import { BackendContractError, contractError } from '../../backend-contract/errors'
import type { PublicOperationOptions } from '../../backend-contract/operations'
import { capacity } from '../../backend-contract/primitives'
import type { BoundedAsyncStream } from '../../backend-contract/streams'
import { CoreBoundedStream } from '../../core/bounded-stream'
import { OwnedCoreBoundedStream } from '../../core/owned-bounded-stream'
import type {
  PeerSecurityEvent,
  PeerSecurityState,
  SecurityBackend,
  SecurityCancelPairingResult,
  SecurityPairOptions,
  SecurityPairResult,
  SecurityUnpairResult
} from '../../backend-contract/security'
import type {
  AndroidSecurityState,
  ReactNativeAndroidProtocolBoundary
} from '../../native-protocol/rn-android-boundary'

const limits = Object.freeze({
  itemCapacity: capacity(16),
  byteCapacity: capacity(16 * 1024),
  reservedControlCapacity: capacity(1)
})
const limitations = Object.freeze([
  Object.freeze({
    code: 'android-link-security-measurement-unavailable',
    explanation:
      'The Android public bond API reports bond state; encryption, authentication, and Secure Connections are not inferred.',
    affectedGuarantee: 'encryption, authentication, and Secure Connections measurement'
  })
])

export interface SecurityStreamOwnershipSnapshot {
  readonly peerCount: number
  readonly streamCount: number
}

const androidSecurityOwnershipInspectors = new WeakMap<
  ReactNativeAndroidSecurityBackend,
  () => SecurityStreamOwnershipSnapshot
>()

export function inspectAndroidSecurityStreamOwnershipForTests(
  backend: ReactNativeAndroidSecurityBackend
): SecurityStreamOwnershipSnapshot {
  const inspect = androidSecurityOwnershipInspectors.get(backend)
  if (inspect === undefined) {
    throw new Error('android security ownership inspector is missing')
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

export class ReactNativeAndroidSecurityBackend implements SecurityBackend {
  private readonly streams = new Map<string, Set<CoreBoundedStream<PeerSecurityEvent>>>()
  private readonly active = new Set<string>()
  private readonly activeNativeIds = new Map<string, string>()
  private readonly sequences = new Map<string, number>()
  private readonly removeListener: () => void
  private closed = false

  constructor(
    private readonly boundary: ReactNativeAndroidProtocolBoundary,
    private readonly now: () => number,
    private readonly nativePeerIdForPeerId: (peerId: string, operation: string) => string = peerId => peerId,
    private readonly peerIdForNativePeerId: (nativePeerId: string) => string | null = nativePeerId => nativePeerId
  ) {
    this.removeListener = boundary.onSecurityState(record => {
      const peerId = this.peerIdForNativePeerId(record.nativePeerId)
      if (peerId !== null) this.emit(peerId, this.snapshot(record.state))
    })
    androidSecurityOwnershipInspectors.set(this, () => securityStreamOwnershipSnapshot(this.streams))
  }

  async state(peerId: string, options: PublicOperationOptions): Promise<PeerSecurityState> {
    this.assertOpen('android.security.state')
    if (options.signal?.aborted === true) {
      throw contractError('operation.aborted', 'core', 'android.security.state')
    }
    if (options.deadline !== null && options.deadline <= this.now()) {
      throw contractError('operation.timed-out', 'core', 'android.security.state')
    }
    const nativePeerId = this.nativePeerIdForPeerId(peerId, 'android.security.state')
    const operation = this.boundary.securityState(nativePeerId)
    return this.snapshot(await settleAndroidOperation(operation, options, this.now, 'android.security.state'))
  }

  watch(peerId: string): BoundedAsyncStream<PeerSecurityEvent> {
    this.assertOpen('android.security.watch')
    const stream = new OwnedCoreBoundedStream<PeerSecurityEvent>(limits, 'error', () => {
      this.removeStream(peerId, stream)
    })
    const streams = this.streams.get(peerId) ?? new Set<CoreBoundedStream<PeerSecurityEvent>>()
    streams.add(stream)
    this.streams.set(peerId, streams)
    this.state(peerId, { signal: null, deadline: null }).then(
      state => this.emit(peerId, state),
      () => {
        stream.closeWithReason('source-failed')
        this.removeStream(peerId, stream)
      }
    )
    return stream
  }

  async pair(peerId: string, options: SecurityPairOptions): Promise<SecurityPairResult> {
    this.assertOpen('android.security.pair')
    if (options.ceremony !== 'system') {
      throw contractError('capability.unsupported', 'capability', 'android.security.custom-ceremony')
    }
    if (options.protection !== 'system-default') {
      throw contractError('capability.unsupported', 'capability', 'android.security.pair.protection')
    }
    if (options.secureConnections !== undefined && options.secureConnections !== 'prefer') {
      // Android's createBond does not expose LE pairing-generation selection, so
      // a 'require' or 'disallow' request cannot be honoured. Fail closed rather
      // than bond without the requested generation.
      throw contractError('capability.unsupported', 'capability', 'android.security.pair.secure-connections')
    }
    if (options.signal?.aborted === true) {
      throw contractError('operation.aborted', 'core', 'android.security.pair')
    }
    if (options.deadline !== null && options.deadline <= this.now()) {
      throw contractError('operation.timed-out', 'core', 'android.security.pair')
    }
    if (this.active.has(peerId))
      throw contractError('ownership.denied', 'platform', 'android.security.pair.arbitration')
    const nativePeerId = this.nativePeerIdForPeerId(peerId, 'android.security.pair')
    this.active.add(peerId)
    this.activeNativeIds.set(peerId, nativePeerId)
    let publicSettled = false
    const cancellationController = new AbortController()
    let abortListener: (() => void) | null = null
    let deadlineTimer: ReturnType<typeof setTimeout> | null = null
    const nativeOperation = this.boundary.pair(nativePeerId, cancellationController.signal)
    const settleNative = (): void => {
      this.active.delete(peerId)
      this.activeNativeIds.delete(peerId)
      if (abortListener !== null) options.signal?.removeEventListener('abort', abortListener)
      if (deadlineTimer !== null) clearTimeout(deadlineTimer)
    }
    const completion = new Promise<SecurityPairResult>((resolve, reject) => {
      const resolveCancelled = (): void => {
        if (publicSettled) return
        publicSettled = true
        resolve({ outcome: 'cancelled' })
      }
      abortListener = (): void => {
        cancellationController.abort()
        if (!this.boundary.securityCancellationAvailable) {
          if (publicSettled) return
          publicSettled = true
          reject(contractError('capability.unsupported', 'capability', 'android.security.pair.cancellation'))
          return
        }
        resolveCancelled()
        this.requestCancellation(peerId)
      }
      options.signal?.addEventListener('abort', abortListener, { once: true })
      if (options.deadline !== null) {
        deadlineTimer = setTimeout(
          () => {
            if (publicSettled) return
            publicSettled = true
            cancellationController.abort()
            if (!this.boundary.securityCancellationAvailable) {
              // Without native cancellation we cannot stop an in-flight
              // createBond at the deadline, so a 'timed-out' outcome (which the
              // public layer maps to 'cancelled') could strand a bond that then
              // completes. Fail closed, exactly as the abort path does.
              reject(contractError('capability.unsupported', 'capability', 'android.security.pair.cancellation'))
              return
            }
            reject(contractError('operation.timed-out', 'core', 'android.security.pair'))
            this.requestCancellation(peerId)
          },
          Math.max(0, options.deadline - this.now())
        )
      }
      nativeOperation.then(
        result => {
          settleNative()
          if (publicSettled) return
          publicSettled = true
          if (result.outcome === 'rejected') resolve({ outcome: 'rejected', reason: null })
          else resolve({ outcome: result.outcome, state: this.snapshot(result.state) })
        },
        error => {
          settleNative()
          if (publicSettled) return
          publicSettled = true
          if (error instanceof BackendContractError && error.normalized.code === 'operation.aborted') {
            resolve({ outcome: 'cancelled' })
          } else if (
            error instanceof BackendContractError &&
            error.normalized.code === 'platform.failure' &&
            error.normalized.platform?.code === 'pairRejected'
          ) {
            resolve({ outcome: 'rejected', reason: error.normalized.platform.safeMessage })
          } else {
            reject(error)
          }
        }
      )
    })
    return completion
  }

  async cancelPairing(peerId: string, _options: PublicOperationOptions): Promise<SecurityCancelPairingResult> {
    this.assertOpen('android.security.cancel-pairing')
    if (!this.boundary.securityCancellationAvailable) {
      throw contractError('capability.unsupported', 'capability', 'android.security.cancel-pairing')
    }
    if (!this.active.has(peerId)) return { outcome: 'not-pairing' }
    const nativePeerId = this.activeNativeIds.get(peerId)
    if (nativePeerId === undefined) return { outcome: 'not-pairing' }
    await this.boundary.cancelPairing(nativePeerId)
    return { outcome: 'cancelled' }
  }

  async unpair(peerId: string, _options: PublicOperationOptions): Promise<SecurityUnpairResult> {
    this.assertOpen('android.security.unpair')
    const nativePeerId = this.nativePeerIdForPeerId(peerId, 'android.security.unpair')
    await this.boundary.unpair(nativePeerId)
    return { outcome: 'unsupported' }
  }

  close(): void {
    this.closed = true
    this.removeListener()
    this.active.clear()
    this.activeNativeIds.clear()
    for (const streams of [...this.streams.values()]) {
      for (const stream of [...streams]) stream.closeWithReason('owner-released')
    }
    this.streams.clear()
  }

  private snapshot(state: AndroidSecurityState): PeerSecurityState {
    return Object.freeze({
      ...state,
      measuredAtMonotonicMs: this.now(),
      limitations
    })
  }

  private requestCancellation(peerId: string): void {
    const nativePeerId = this.activeNativeIds.get(peerId)
    if (nativePeerId === undefined) return
    this.boundary.cancelPairing(nativePeerId).catch(error => {
      console.error('[ReactNativeAndroidSecurityBackend.pair] Pair cancellation was not accepted:', error)
    })
  }

  private assertOpen(operation: string): void {
    if (this.closed) throw contractError('lifecycle.destroyed', 'core', operation)
  }

  private emit(peerId: string, state: PeerSecurityState): void {
    const streams = this.streams.get(peerId)
    if (streams === undefined) return
    const sequence = (this.sequences.get(peerId) ?? 0) + 1
    this.sequences.set(peerId, sequence)
    const event = Object.freeze({ kind: 'state' as const, peerId, sequence, state })
    for (const stream of [...streams]) if (stream.emit(event, 1).terminated) streams.delete(stream)
    if (streams.size === 0) this.streams.delete(peerId)
  }

  private removeStream(peerId: string, stream: CoreBoundedStream<PeerSecurityEvent>): void {
    const streams = this.streams.get(peerId)
    if (streams === undefined) return
    streams.delete(stream)
    if (streams.size === 0) this.streams.delete(peerId)
  }
}

function settleAndroidOperation<Value>(
  operation: Promise<Value>,
  options: PublicOperationOptions,
  now: () => number,
  operationName: string
): Promise<Value> {
  return new Promise((resolve, reject) => {
    let settled = false
    let timer: ReturnType<typeof setTimeout> | null = null
    const clear = (): void => {
      options.signal?.removeEventListener('abort', abort)
      if (timer !== null) {
        clearTimeout(timer)
        timer = null
      }
    }
    const fail = (error: Error): void => {
      if (settled) return
      settled = true
      clear()
      reject(error)
    }
    const abort = (): void => fail(contractError('operation.aborted', 'core', operationName))
    options.signal?.addEventListener('abort', abort, { once: true })
    if (options.deadline !== null) {
      timer = setTimeout(
        () => fail(contractError('operation.timed-out', 'core', operationName)),
        Math.max(0, options.deadline - now())
      )
    }
    operation.then(
      value => {
        if (settled) return
        settled = true
        clear()
        resolve(value)
      },
      error => fail(error instanceof Error ? error : new Error('Android security state operation failed'))
    )
  })
}
