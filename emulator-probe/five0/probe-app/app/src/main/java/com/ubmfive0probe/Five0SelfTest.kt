package com.ubmfive0probe

import android.Manifest
import android.app.Activity
import android.content.pm.PackageManager
import android.os.Build
import android.util.Log
import com.ubm.echo.EchoBridge
import com.ubm.echo.EchoException
import com.ubm.gatt.GattBridge

/**
 * five0 in-APK self-test (UBM 5.0 HOST-ANDROID slice).
 *
 * Runs on a worker thread at launch (never the main thread): loads the Rust
 * cdylib, checks the binding identity, enforces the BLE permission gate
 * fail-closed, then drives a full GATT cycle through the REAL Central with
 * synthetic wires (SIMULATED-injection: no BLE peers exist on the emulator;
 * the virtual-radio boundary means these prove the bridge↔core path, never
 * physical radio behavior). Exactly one `UBM5FIVE0-RESULT` JSON line is
 * logged for the host-side battery to match; every check is also logged.
 */
object Five0SelfTest {
  const val TAG = "UBM5FIVE0"
  const val REV = "C-UBM.0.1.2-DRAFT"

  fun run(activity: Activity): String {
    val failures = mutableListOf<String>()
    var checks = 0
    fun check(name: String, cond: Boolean, extra: String = "") {
      checks++
      Log.i(TAG, "check $name ${if (cond) "PASS" else "FAIL"} $extra")
      if (!cond) failures.add(name)
    }

    try {
      System.loadLibrary("ubm5_jni_echo")
    } catch (err: UnsatisfiedLinkError) {
      val json = resultJson(checks = 0, failures = listOf("native-lib-load"),
        result = "failed",
        detail = "UnsatisfiedLinkError:" + oneLine(err.message ?: "unknown"))
      Log.e(TAG, "UBM5FIVE0-RESULT $json")
      return json
    }
    check("native-lib-load", true)

    val revision = try {
      EchoBridge.nativeRevision()
    } catch (err: EchoException) {
      failures.add("binding-identity: ${err.message}")
      ""
    }
    check("binding-identity", revision == REV, revision)

    // Permission gate (fail-closed): without runtime BLE permission the core
    // cycle must NOT run; the denial identity is the observation.
    val granted = hasBlePermissions(activity)
    if (!granted) {
      val json = resultJson(checks, failures, "permission-denied:fail-closed",
        "permission.denied|rn-android-boundary|five0-gate|ble-permission-missing")
      Log.w(TAG, "UBM5FIVE0-PERMISSION $json")
      Log.i(TAG, "UBM5FIVE0-RESULT $json")
      return json
    }
    check("permission-granted", true)

    var handle = 0L
    try {
      handle = EchoBridge.nativeOpen(REV)
      check("session-open", handle != 0L, "handle=$handle")

      val scanLine = drain1(handle, "scan.start|five0|5000|1000||all|none",
        "scan-admitted", failures)
      checks++
      val scanOp = opOf(scanLine, failures, "scan-op") ?: ""
      val startedLine = drain(handle, "scan.platform-started|$scanOp", failures)
      checks++
      check("scan-platform-started", startedLine.contains("\"ok\":true"), startedLine)
      val peerLine = drain(handle, "peer.resolve|public-address|AA:BB:CC:DD:EE:FF", failures)
      checks++
      val peer = peerLine.substringAfter("\"peer\":\"").substringBefore("\"")
      check("peer-resolved", peer.startsWith("public-address:"), peer)

      val connectOp = opOf(drain(handle, "connect|$peer|five0-lease|5000|1000", failures),
        failures, "connect-op") ?: ""
      checks++
      drain(handle, "link.established|$peer", failures).also { checks++ }
      drain(handle, "discovery.begin|$peer", failures).also { checks++ }
      drain(handle, "discovery.complete|$peer", failures).also { checks++ }
      val pathLine = drain(handle,
        "path.register|$peer|180d|0|2a37|0|-|-|11|five0-lease", failures)
      checks++
      check("path-registered", pathLine.contains("\"path\":0"), pathLine)

      val readOp = opOf(drain(handle, "read.start|0|5000|1000", failures),
        failures, "read-op") ?: ""
      checks++
      drain(handle, "op.dispatch|$readOp", failures).also { checks++ }
      val settled = drain(handle, "op.settle|$readOp|success|true|7|1000", failures)
      checks++
      check("io-settled", settled.contains("succeeded"), settled)

      val read2Op = opOf(drain(handle, "read.start|0|5000|1000", failures),
        failures, "read2-op") ?: ""
      checks++
      val cancelled = drain(handle, "op.cancel|$read2Op|1000", failures)
      checks++
      check("queued-cancel-aborts", cancelled.contains("aborted"), cancelled)

      drain(handle, "subscribe|0|error|8|1024|five0-consumer|5000|1000", failures).also { checks++ }
      drain(handle, "subscribe.enable-settled|0|true|1000", failures).also { checks++ }
      val delivered = drain(handle, "notify.deliver|0|0102", failures)
      checks++
      check("notify-delivered", delivered.contains("\"outcome\":\"delivered\""), delivered)

      try {
        GattBridge.nativeEnqueueGattEvent(handle, "notify.deliver|0|" + "ab".repeat(600000))
        check("notify-oversize-rejects", false, "no exception")
      } catch (err: EchoException) {
        checks++
        check("notify-oversize-rejects", err.message == "bytes.too-large|core|gatt-enqueue|event-exceeds-wire-max",
          err.message ?: "")
      }

      drain(handle, "services-changed|$peer", failures).also { checks++ }
      val stale = drainRaw(handle, "read.start|0|5000|1000", failures)
      checks++
      check("stale-handle-fails-closed",
        stale.contains("\"ok\":false") && stale.contains("gatt.stale-handle"), stale)

      val reset = drain(handle, "adapter.reset|2000", failures)
      checks++
      check("adapter-reset", reset.contains("\"settled\""), reset)

      val sweep = drain(handle, "expire-sweep|2000", failures)
      checks++
      check("expire-sweep", sweep.contains("\"settled\""), sweep)

      val released = drain(handle, "release", failures)
      checks++
      check("release", released.contains("\"state\":\"released\""), released)

      val status = try {
        EchoBridge.nativeCentralStatus(handle)
      } catch (err: EchoException) {
        failures.add("central-status: ${err.message}"); ""
      }
      checks++
      check("central-status", status.contains(REV), status)

      EchoBridge.nativeClose(handle)
      handle = 0L
      try {
        GattBridge.nativeEnqueueGattEvent(handle, "release")
        check("post-close-enqueue-rejects", false, "no exception")
      } catch (err: EchoException) {
        checks++
        check("post-close-enqueue-rejects",
          err.message == "lifecycle.destroyed|core|gatt-enqueue|unknown-or-closed-handle",
          err.message ?: "")
      }
    } catch (err: EchoException) {
      failures.add("unexpected-throw: ${err.message}")
    } finally {
      if (handle != 0L) {
        try {
          EchoBridge.nativeClose(handle)
        } catch (_: EchoException) {
        }
      }
    }

    val json = resultJson(checks, failures, if (failures.isEmpty()) "ok" else "failed", "")
    Log.i(TAG, "UBM5FIVE0-RESULT $json")
    return json
  }

  private fun hasBlePermissions(activity: Activity): Boolean {
    if (Build.VERSION.SDK_INT >= 31) {
      return activity.checkSelfPermission(Manifest.permission.BLUETOOTH_CONNECT) ==
        PackageManager.PERMISSION_GRANTED &&
        activity.checkSelfPermission(Manifest.permission.BLUETOOTH_SCAN) ==
        PackageManager.PERMISSION_GRANTED
    }
    return activity.checkSelfPermission(Manifest.permission.ACCESS_FINE_LOCATION) ==
      PackageManager.PERMISSION_GRANTED
  }

  private fun drain(handle: Long, wire: String, failures: MutableList<String>): String {
    GattBridge.nativeEnqueueGattEvent(handle, wire)
    val out = GattBridge.nativeDrainGattEvents(handle)
    if (!out.contains("\"ok\":true")) {
      failures.add("drain-not-ok: $wire -> $out")
    }
    return out
  }

  private fun drain1(
    handle: Long,
    wire: String,
    label: String,
    failures: MutableList<String>
  ): String {
    val out = drain(handle, wire, failures)
    if (!out.contains("\"ok\":true")) failures.add("$label: $out")
    return out
  }

  private fun drainRaw(handle: Long, wire: String, failures: MutableList<String>): String {
    try {
      GattBridge.nativeEnqueueGattEvent(handle, wire)
      return GattBridge.nativeDrainGattEvents(handle)
    } catch (err: EchoException) {
      failures.add("drainRaw-threw: $wire -> ${err.message}")
      return ""
    }
  }

  private fun opOf(line: String, failures: MutableList<String>, label: String): String? {
    val key = "\"op\":\""
    val start = line.indexOf(key)
    if (start < 0) {
      failures.add("$label: no op in $line")
      return null
    }
    val from = start + key.length
    val end = line.indexOf('"', from)
    if (end < 0) {
      failures.add("$label: unterminated op in $line")
      return null
    }
    return line.substring(from, end)
  }

  private fun resultJson(
    checks: Int,
    failures: List<String>,
    result: String,
    detail: String
  ): String {
    val safe = failures.joinToString(";").replace("\"", "'").take(800)
    return "{\"result\":\"$result\",\"checks\":$checks,\"failures\":\"$safe\"," +
      "\"detail\":\"${detail.replace("\"", "'").take(300)}\"}"
  }

  private fun oneLine(text: String): String =
    text.replace("\n", " ").replace("\"", "'").take(200)
}
