// src/backends/reactnative/react-native-rust-core-drain.ts
//
// The one delivery path from the Rust mobile owner to JavaScript (PR210-17):
// a wake-driven, single-flight drain. The owner wakes the session once when
// its outbox turns non-empty; JS then drains `(256, 65536)` batches until
// `more` is false. A wake that arrives mid-drain schedules one more pass, so
// a record that raced the owner's re-arm is never stranded. There is no
// timer and no polling: an idle session costs zero native calls.

import type { ReactNativeRustCoreSession } from './react-native-rust-core'
import type { WireDrainRecord } from './rust-core-wire'

/** Records per drain call (docs/MOBILE_RUST_WIRE.md "Threading and wakeups"). */
export const DRAIN_MAX_ITEMS = 256
/** Bytes per drain call. */
export const DRAIN_MAX_BYTES = 65536

export interface RustCoreDrainSink {
  /** Delivers one batch in ordinal order. A throw is a delivery defect and ends the router. */
  deliver(records: readonly WireDrainRecord[]): void
  /** The session can no longer be drained; `error` is what the owner reported. */
  failed(error: unknown): void
}

export class RustCoreDrainRouter {
  private draining: Promise<void> | null = null
  private again = false
  private stopped = false
  private removeWake: (() => void) | null = null

  constructor(
    private readonly session: ReactNativeRustCoreSession,
    private readonly sink: RustCoreDrainSink
  ) {}

  /** Registers for wakes and drains once, collecting anything queued before registration. */
  start(): void {
    this.removeWake = this.session.onWake(() => this.wake())
    this.wake()
  }

  /** One wake: start a drain, or schedule one more pass behind the running one. */
  wake(): void {
    if (this.stopped) return
    if (this.draining !== null) {
      this.again = true
      return
    }
    this.draining = this.run().finally(() => {
      this.draining = null
    })
  }

  /** Stops routing; resolves once an in-flight drain settled. */
  async stop(): Promise<void> {
    this.stopped = true
    this.removeWake?.()
    this.removeWake = null
    await this.draining
  }

  private async run(): Promise<void> {
    try {
      do {
        this.again = false
        let more = true
        while (more && !this.stopped) {
          const batch = await this.session.drain(DRAIN_MAX_ITEMS, DRAIN_MAX_BYTES)
          // Records already taken from the owner are delivered even when a stop
          // raced the call: their streams report their own terminal state.
          this.sink.deliver(batch.records)
          more = batch.more
        }
      } while (this.again && !this.stopped)
    } catch (error) {
      this.stopped = true
      this.removeWake?.()
      this.removeWake = null
      this.sink.failed(error)
    }
  }
}
