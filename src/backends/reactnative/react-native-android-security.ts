import { BackendContractError, contractError } from '../../backend-contract/errors'
import type { PublicOperationOptions } from '../../backend-contract/operations'
import { capacity } from '../../backend-contract/primitives'
import type { BoundedAsyncStream } from '../../backend-contract/streams'
import { CoreBoundedStream } from '../../core/bounded-stream'
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

export class ReactNativeAndroidSecurityBackend implements SecurityBackend {
  private readonly streams = new Map<string, Set<CoreBoundedStream<PeerSecurityEvent>>>()
  private readonly active = new Set<string>()
  private readonly sequences = new Map<string, number>()
  private readonly removeListener: () => void

  constructor(
    private readonly boundary: ReactNativeAndroidProtocolBoundary,
    private readonly now: () => number
  ) {
    this.removeListener = boundary.onSecurityState(record =>
      this.emit(record.nativePeerId, this.snapshot(record.state))
    )
  }

  async state(peerId: string, _options: PublicOperationOptions): Promise<PeerSecurityState> {
    return this.snapshot(await this.boundary.securityState(peerId))
  }

  watch(peerId: string): BoundedAsyncStream<PeerSecurityEvent> {
    const stream = new CoreBoundedStream<PeerSecurityEvent>(limits, 'error')
    const streams = this.streams.get(peerId) ?? new Set<CoreBoundedStream<PeerSecurityEvent>>()
    streams.add(stream)
    this.streams.set(peerId, streams)
    this.state(peerId, { signal: null, deadline: null }).then(
      state => this.emit(peerId, state),
      () => stream.closeWithReason('source-failed')
    )
    return stream
  }

  async pair(peerId: string, options: SecurityPairOptions): Promise<SecurityPairResult> {
    if (options.ceremony !== 'system') {
      throw contractError('capability.unsupported', 'capability', 'android.security.custom-ceremony')
    }
    if (options.signal?.aborted === true) {
      throw contractError('operation.aborted', 'core', 'android.security.pair')
    }
    if (options.deadline !== null && options.deadline <= this.now()) {
      throw contractError('operation.timed-out', 'core', 'android.security.pair')
    }
    if (!this.active.add(peerId))
      throw contractError('ownership.denied', 'platform', 'android.security.pair.arbitration')
    let publicSettled = false
    let abortListener: (() => void) | null = null
    let deadlineTimer: ReturnType<typeof setTimeout> | null = null
    const nativeOperation = this.boundary.pair(peerId)
    const settleNative = (): void => {
      this.active.delete(peerId)
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
        resolveCancelled()
        if (this.boundary.securityCancellationAvailable) this.requestCancellation(peerId)
      }
      options.signal?.addEventListener('abort', abortListener, { once: true })
      if (options.deadline !== null) {
        deadlineTimer = setTimeout(
          () => {
            if (publicSettled) return
            publicSettled = true
            reject(contractError('operation.timed-out', 'core', 'android.security.pair'))
            if (this.boundary.securityCancellationAvailable) this.requestCancellation(peerId)
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
    if (!this.active.has(peerId)) return { outcome: 'not-pairing' }
    await this.boundary.cancelPairing(peerId)
    return { outcome: 'cancelled' }
  }

  async unpair(peerId: string, _options: PublicOperationOptions): Promise<SecurityUnpairResult> {
    await this.boundary.unpair(peerId)
    return { outcome: 'unsupported' }
  }

  close(): void {
    this.removeListener()
    this.active.clear()
    for (const streams of this.streams.values()) for (const stream of streams) stream.closeWithReason('owner-released')
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
    this.boundary.cancelPairing(peerId).catch(error => {
      console.error('[ReactNativeAndroidSecurityBackend.pair] Pair cancellation was not accepted:', error)
    })
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
}
