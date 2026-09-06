import type { CleanupRecord, NormalizedBleError } from '../backend-contract/errors'
import type { OverflowPolicy, StreamLimits } from '../backend-contract/streams'
import { CoreBoundedStream, type CoreStreamTerminalReason } from './bounded-stream'

/**
 * Backend-owned fan-out stream whose registry callback runs from close, terminal,
 * and overflow-error dispatch. Release is idempotent so iterator close and producer
 * terminal cannot leave a live registry entry.
 */
export class OwnedCoreBoundedStream<Value> extends CoreBoundedStream<Value> {
  private ownershipReleased = false

  constructor(
    limits: StreamLimits,
    overflowPolicy: OverflowPolicy,
    private readonly releaseOwnership: () => void
  ) {
    super(limits, overflowPolicy)
  }

  override close(): Promise<CleanupRecord> {
    return Promise.resolve(super.close()).then(cleanup => {
      if (cleanup.state === 'released') {
        this.release()
      }
      return cleanup
    })
  }

  override closeWithReason(reason: CoreStreamTerminalReason, error: NormalizedBleError | null = null): void {
    super.closeWithReason(reason, error)
    this.release()
  }

  override finishWithReason(reason: CoreStreamTerminalReason, error: NormalizedBleError | null = null): void {
    super.finishWithReason(reason, error)
    this.release()
  }

  private release(): void {
    if (this.ownershipReleased) {
      return
    }
    this.ownershipReleased = true
    this.releaseOwnership()
  }
}
