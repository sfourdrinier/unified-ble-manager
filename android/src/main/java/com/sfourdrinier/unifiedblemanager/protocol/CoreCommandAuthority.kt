// android/src/main/java/com/sfourdrinier/unifiedblemanager/protocol/CoreCommandAuthority.kt

package com.sfourdrinier.unifiedblemanager.protocol

/**
 * R02 Android authority table: which protocol commands the shared core admits.
 *
 * Authority means core verdicts BIND: a covered command executes the platform
 * radio only after the core admits the same transition
 * ([UbmGattCentralBridge.PostResult.Queued]), and a core drain rejection
 * (`ok:false`) for a correlated wire event fails a still-pending covered
 * command instead of leaving a radio success standing. A missing/failed
 * shadow or a refused post fails the command loud with a `core*`-coded
 * ERROR terminal — never silent radio-only execution.
 *
 * Covered commands (admission-gated by
 * `UnifiedBleProtocolAndroidDispatcher`):
 * - `scanStart` / `scanStop` — `scan.start` / `scan.stop`
 *   ([UbmGattCoreBinding.postScanStart]/[postScanStop]).
 * - `connect` — `peer.resolve` + `connect`
 *   ([UbmGattCoreBinding.postConnect]); `link.established` outcome reports
 *   bind a still-pending connect the same way.
 * - `disconnect` — `disconnect` ([UbmGattCoreBinding.postDisconnect]).
 * - `discover` — `discovery.begin` ([UbmGattCoreBinding.postDiscoveryBegin]);
 *   `discovery.complete` / `discovery.fail` outcome reports bind a
 *   still-pending discover the same way.
 *
 * Scoped exceptions (radio-only, NOT faked as core authority):
 * - (A) Unwired surface: `read`, `write`, `readDescriptor`,
 *   `writeDescriptor`, `subscribe`, `unsubscribe`. Native verbs exist
 *   (`path.register`, `read.start`, `write.start`, `op.dispatch`,
 *   `op.settle`, `op.cancel`, `subscribe`, `unsubscribe`,
 *   `notify.deliver` — see `com.ubm.gatt.GattBridge` and
 *   `bindings/jni/src/gatt_queue.rs`), but the Android binding exposes no
 *   path/lease-context posts for them yet (see the follow-up note on
 *   [UbmGattCoreBinding]). Promoting them without that context would fake
 *   attestation, so they stay radio-only until the binding wires them.
 * - (B) No native op surface: `readRssi`, `requestMtu`, `readMtu`,
 *   `requestPriority`, `readPhy`, `requestPhy` (core `CENTRAL_CONTROLS` rows
 *   are acceptance-only capability flags with no JNI wire verb), and
 *   `securityState`, `securityPair`, `securityCancelPairing`,
 *   `enumerateBondedPeers` (core security methods exist but have no
 *   `gatt_queue` wire arm). Nothing to admit through: radio-only.
 * - (C) Local lifecycle: `cancel` (no core op id is tracked to cancel) and
 *   `destroy`/`close` (teardown drives the `release` transition
 *   unconditionally rather than seeking admission).
 *
 * Event-path mirrors (`adapter.reset`, `services-changed`,
 * `link.established`/`link.released` outside a pending command, `peer.loss`,
 * discovery outcome reports after their command settled) stay fire-and-forget
 * with diagnostics: they report physical facts, they gate no command.
 *
 * Attribution limit: a core rejection line carries no peer or dispatcher op
 * key (`{"ok":false,"event":...,"code":...}`), so binding is by command kind
 * for commands admitted before the rejection (sequence-guarded). A rejection
 * can fail a same-kind concurrent command for another peer — loud and safe
 * (never a false success), never silent.
 */
internal object CoreCommandAuthority {
  /** Shadow missing/failed/withdrawn at command time; the command never ran. */
  const val CODE_UNAVAILABLE = "coreUnavailable"

  /** The bridge refused the post before JNI: BLE permission missing. */
  const val CODE_PERMISSION_DENIED = "corePermissionDenied"

  /** The enqueue call itself failed; the line never reached the core. */
  const val CODE_ENQUEUE_FAILED = "coreEnqueueFailed"

  /** The line queued but no drain was arranged; attestation cannot complete. */
  const val CODE_SCHEDULE_FAILED = "coreScheduleFailed"

  /** The core drained `ok:false` for the command's wire event; binds. */
  const val CODE_REJECTED = "coreRejected"

  /** Commands that must be admitted by the core before radio execution. */
  val ADMISSION_COMMANDS: Set<String> = setOf(
    "scanStart",
    "scanStop",
    "connect",
    "disconnect",
    "discover"
  )

  private val COMMAND_TO_EVENTS: Map<String, Set<String>> = mapOf(
    "scanStart" to setOf("scan.start"),
    "scanStop" to setOf("scan.stop"),
    "connect" to setOf("peer.resolve", "connect", "link.established"),
    "disconnect" to setOf("disconnect"),
    "discover" to setOf("discovery.begin", "discovery.complete", "discovery.fail")
  )

  private val EVENT_TO_COMMANDS: Map<String, Set<String>> = buildMap {
    COMMAND_TO_EVENTS.forEach { (command, events) ->
      events.forEach { event ->
        merge(event, setOf(command)) { left, right -> left + right }
      }
    }
  }

  /** True when [commandKind] must be admitted by the core before radio work. */
  fun requiresAdmission(commandKind: String): Boolean = commandKind in ADMISSION_COMMANDS

  /**
   * Core wire events correlated to [commandKind] (admission lines plus the
   * outcome reports that bind while the command is still pending).
   */
  fun coreEventsFor(commandKind: String): Set<String> =
    COMMAND_TO_EVENTS[commandKind] ?: emptySet()

  /**
   * Pending command kinds bound by a rejection of core wire [event].
   * Empty for pure event-path lines (adapter reset, peer loss,
   * services-changed, link release): those diagnose only.
   */
  fun commandKindsForCoreEvent(event: String): Set<String> =
    EVENT_TO_COMMANDS[event] ?: emptySet()
}
