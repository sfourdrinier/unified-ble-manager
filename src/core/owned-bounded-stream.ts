import type { CleanupRecord } from '../backend-contract/errors'
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

  override closeWithReason(reason: CoreStreamTerminalReason): void {
    super.closeWithReason(reason)
    this.release()
  }

  override finishWithReason(reason: CoreStreamTerminalReason): void {
    super.finishWithReason(reason)
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
