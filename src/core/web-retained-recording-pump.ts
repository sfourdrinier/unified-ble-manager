import { contractError } from '../backend-contract/errors'
import type { WebIngressRecord, WebRetainedIngressGroup } from '../web/web-notification-ingress'

/** Commit resolves only after the IndexedDB transaction completes. Repeating a
 * delivery must compare its complete payload against the retained identity.
 * When available, its exact journal ordinal is passed to the ACK barrier. */
export interface WebRetainedRecordingJournal {
  commit(delivery: { readonly id: string; readonly record: WebIngressRecord }): Promise<void | number>
  retire(id: string): Promise<void>
  /** Synchronous epoch fence immediately before source acknowledgement. */
  canAcknowledge?(): boolean
  /** Persist evidence of the exact source acknowledgement. Retries must be idempotent. */
  acknowledged?(delivery: WebRetainedAcknowledgedDelivery): void | Promise<void>
}

export interface WebRetainedAcknowledgedDelivery {
  readonly id: string
  readonly record: WebIngressRecord
  readonly journalOrdinal?: number
}

/** Serializes source acknowledgement and identity retirement behind storage. */
export class WebRetainedRecordingPump {
  private inFlight: Promise<'empty' | 'committed' | 'retired'> | null = null
  private acknowledgedDelivery: {
    readonly delivery: WebRetainedAcknowledgedDelivery
    evidenceRecorded: boolean
  } | null = null
  private invalidHead = false

  constructor(
    private readonly source: WebRetainedIngressGroup,
    private readonly journal: WebRetainedRecordingJournal
  ) {}

  hasPendingAcknowledgement(): boolean {
    return this.acknowledgedDelivery !== null
  }

  runOnce(): Promise<'empty' | 'committed' | 'retired'> {
    if (this.inFlight !== null) return this.inFlight
    // Install the in-flight receipt before invoking any host-supplied code.
    const run = Promise.resolve().then(() => this.step())
    this.inFlight = run
    run.then(
      () => {
        this.inFlight = null
      },
      () => {
        this.inFlight = null
      }
    )
    return run
  }

  private async step(): Promise<'empty' | 'committed' | 'retired'> {
    if (this.journal.canAcknowledge?.() === false) throw new Error('Web recording authority lost')
    if (this.invalidHead) throw contractError('lifecycle.invariant-violation', 'stream', 'web-recording.ack-head')
    if (this.acknowledgedDelivery !== null) {
      await this.finishAcknowledgement()
      return 'retired'
    }
    const delivery = this.source.peek()
    if (delivery === null) return 'empty'
    const journalOrdinal = await this.journal.commit(delivery)
    if (this.journal.canAcknowledge?.() === false) throw new Error('Web recording authority lost')
    if (!this.source.ackExact(delivery.id)) {
      this.invalidHead = true
      throw contractError('lifecycle.invariant-violation', 'stream', 'web-recording.ack-head')
    }
    this.acknowledgedDelivery = {
      delivery: journalOrdinal === undefined ? delivery : { ...delivery, journalOrdinal },
      evidenceRecorded: false
    }
    await this.finishAcknowledgement()
    return 'committed'
  }

  private async finishAcknowledgement(): Promise<void> {
    const pending = this.acknowledgedDelivery
    if (pending === null) throw contractError('lifecycle.invariant-violation', 'stream', 'web-recording.ack-head')
    if (!pending.evidenceRecorded) {
      await this.journal.acknowledged?.(pending.delivery)
      pending.evidenceRecorded = true
    }
    if (this.journal.canAcknowledge?.() === false) throw new Error('Web recording authority lost')
    await this.journal.retire(pending.delivery.id)
    this.acknowledgedDelivery = null
  }
}
