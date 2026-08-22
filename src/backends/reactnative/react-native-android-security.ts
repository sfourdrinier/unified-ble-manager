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
  private closed = false

  constructor(
    private readonly boundary: ReactNativeAndroidProtocolBoundary,
    private readonly now: () => number
  ) {
    this.removeListener = boundary.onSecurityState(record =>
      this.emit(record.nativePeerId, this.snapshot(record.state))
    )
  }

  async state(peerId: string, _options: PublicOperationOptions): Promise<PeerSecurityState> {
    this.assertOpen('android.security.state')
    return this.snapshot(await this.boundary.securityState(peerId))
  }

  watch(peerId: string): BoundedAsyncStream<PeerSecurityEvent> {
    this.assertOpen('android.security.watch')
    const stream = new CoreBoundedStream<PeerSecurityEvent>(limits, 'error')
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
    if (options.signal?.aborted === true) {
      throw contractError('operation.aborted', 'core', 'android.security.pair')
    }
    if (options.deadline !== null && options.deadline <= this.now()) {
      throw contractError('operation.timed-out', 'core', 'android.security.pair')
    }
    if (this.active.has(peerId))
      throw contractError('ownership.denied', 'platform', 'android.security.pair.arbitration')
    this.active.add(peerId)
    let publicSettled = false
    const cancellationController = new AbortController()
    let abortListener: (() => void) | null = null
    let deadlineTimer: ReturnType<typeof setTimeout> | null = null
    const nativeOperation = this.boundary.pair(peerId, cancellationController.signal)
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
        cancellationController.abort()
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
    if (!this.active.has(peerId)) return { outcome: 'not-pairing' }
    await this.boundary.cancelPairing(peerId)
    return { outcome: 'cancelled' }
  }

  async unpair(peerId: string, _options: PublicOperationOptions): Promise<SecurityUnpairResult> {
    this.assertOpen('android.security.unpair')
    await this.boundary.unpair(peerId)
    return { outcome: 'unsupported' }
  }

  close(): void {
    this.closed = true
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
    this.boundary.cleanupPairing(peerId).catch(error => {
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
