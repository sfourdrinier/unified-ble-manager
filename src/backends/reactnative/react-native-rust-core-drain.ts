// src/backends/reactnative/react-native-rust-core-drain.ts
//
// The one delivery path from the Rust mobile owner to JavaScript (PR210-17):
// a wake-driven, single-flight drain. The owner wakes the session once when
// its outbox turns non-empty; JS then drains until `more` is false. A wake
// that arrives mid-drain schedules one more pass, so a record that raced the
// owner's re-arm is never stranded. There is no timer and no polling: an idle
// session costs zero native calls.
//
// A batch is delivered one data record (`adv`, `value`) per native→JS task,
// as the legacy boundary delivered one native callback per record. A stream
// reader re-arms through several promise hops; a `latest` stream holds one
// item, so data records emitted in one synchronous loop overflow it before
// its reader runs (`find` rejected `stream.overflow` on a Samsung device).
// Taken records wait in a FIFO backlog; each delivery pass hands over
// records up to (not including) the second data record. The boundary before
// the next pass is the next drain call, which takes ONE record while a
// backlog remains (so the backlog never grows): a TurboModule promise
// resolves through the JS call invoker as a task of its own, and runs while
// the app is backgrounded. Control records stay in their drained order and
// keep their neighbours (a `link` and the `stream-end`s behind it arrive in
// one pass). No JS timer is a boundary: React Native timers stop with the
// host (Android Choreographer frames, iOS CADisplayLink), which held
// notifications ~26 s with the screen off, and bridgeless `setImmediate` is a
// microtask shim.

import type { ReactNativeRustCoreSession } from './react-native-rust-core'
import type { WireDrainRecord } from './rust-core-wire'

/** Records per drain call with no backlog (docs/MOBILE_RUST_WIRE.md "Threading and wakeups"). */
export const DRAIN_MAX_ITEMS = 256
/** Records per drain call while a backlog remains: the call is the task boundary. */
export const DRAIN_BOUNDARY_ITEMS = 1
/** Bytes per drain call (the owner always hands over at least one record). */
export const DRAIN_MAX_BYTES = 65536

export interface RustCoreDrainSink {
  /** Delivers one record, called in ordinal order. A throw is a delivery defect and ends the router. */
  deliver(record: WireDrainRecord): void
  /** The session can no longer be drained; `error` is what the owner reported. */
  failed(error: unknown): void
}

/** Records a stream reader consumes: one per native→JS task. */
function isDataRecord(record: WireDrainRecord): boolean {
  return record.t === 'adv' || record.t === 'value'
}

export class RustCoreDrainRouter {
  /** Records taken from the owner and not yet delivered, in ordinal order. */
  private readonly backlog: WireDrainRecord[] = []
  private draining: Promise<void> | null = null
  private again = false
  private stopped = false
  /** Stop requested: drain what the owner still holds, then end. */
  private stopping = false
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
    if (this.stopped || this.stopping) return
    if (this.draining !== null) {
      this.again = true
      return
    }
    this.draining = this.run().finally(() => {
      this.draining = null
    })
  }

  /**
   * Stops routing. Runs after `session.dispose`, whose owner keeps its outbox:
   * every record it still holds is drained and delivered (a value for a
   * released consumer surfaces as `unmatched-notification`), so teardown
   * accounts for everything the owner queued. Resolves once that flush ends.
   */
  async stop(): Promise<void> {
    this.stopping = true
    this.removeWake?.()
    this.removeWake = null
    if (this.draining === null && !this.stopped) {
      this.draining = this.run().finally(() => {
        this.draining = null
      })
    } else {
      this.again = true
    }
    await this.draining
    this.stopped = true
  }

  private async run(): Promise<void> {
    try {
      do {
        this.again = false
        let more = true
        while ((more || this.backlog.length > 0) && !this.stopped) {
          const batch = await this.session.drain(
            this.backlog.length > 0 ? DRAIN_BOUNDARY_ITEMS : DRAIN_MAX_ITEMS,
            DRAIN_MAX_BYTES
          )
          this.backlog.push(...batch.records)
          more = batch.more
          this.deliverPass()
        }
      } while (this.again && !this.stopped)
    } catch (error) {
      this.stopped = true
      this.removeWake?.()
      this.removeWake = null
      // Records already taken are facts the owner reported: they are
      // delivered before every stream ends with the failure. A sink whose
      // deliver keeps throwing cannot take them; the defect is reported once
      // through failed() below, so the flush stops at the second throw
      // instead of rejecting (the wake path owns no await, and an escaping
      // rejection would be unhandled noise on React Native).
      try {
        while (this.backlog.length > 0) this.deliverPass()
      } catch {
        // The delivery defect is reported once via failed(); nothing else
        // can take the undelivered records.
      }
      this.sink.failed(error)
    }
  }

  /** Delivers the backlog up to, not including, its second data record. */
  private deliverPass(): void {
    let dataDelivered = false
    for (let record = this.backlog[0]; record !== undefined; record = this.backlog[0]) {
      if (isDataRecord(record)) {
        if (dataDelivered) return
        dataDelivered = true
      }
      this.backlog.shift()
      this.sink.deliver(record)
    }
  }
}
