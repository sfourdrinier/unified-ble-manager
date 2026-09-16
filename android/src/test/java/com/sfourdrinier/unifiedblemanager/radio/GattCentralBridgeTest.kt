// android/src/test/java/com/sfourdrinier/unifiedblemanager/radio/GattCentralBridgeTest.kt

package com.sfourdrinier.unifiedblemanager.radio

import java.util.ArrayDeque
import java.util.concurrent.ConcurrentLinkedQueue
import java.util.concurrent.CountDownLatch
import java.util.concurrent.Executor
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test

/**
 * HOST-JVM tests for the HOST-ANDROID GATT bridge runtime (no emulator, no
 * BLE hardware): wire shapes, fail-fast validation, permission fail-closed
 * behavior, binder-thread non-blocking posts, and observation parsing.
 */
class GattCentralBridgeTest {

  private val direct = Executor { command -> command.run() }

  @Test
  fun scanStartWireShape() {
    assertEquals(
      "scan.start|owner-a|5000|1000|180d,180f|all|none",
      GattCentralWire.scanStart("owner-a", 5000, 1000, listOf("180d", "180f"), "all", "none")
    )
  }

  @Test
  fun notifyWireEncodesHexAndBoundsValues() {
    assertEquals(
      "notify.deliver|3|0102ff",
      GattCentralWire.notifyDeliver(3, byteArrayOf(0x01, 0x02, 0xFF.toByte()))
    )
    try {
      GattCentralWire.notifyDeliver(0, ByteArray(GattCentralWire.NOTIFY_MAX_BYTES + 1))
      fail("oversize notify must reject before JNI")
    } catch (expected: IllegalArgumentException) {
      assertTrue(expected.message!!.contains("exceeds"))
    }
    // Hex doubles the size on the wire: a max-size value must still fit the
    // enqueue ceiling, or Kotlin validation would disagree with Rust.
    val maxLine = GattCentralWire.notifyDeliver(0, ByteArray(GattCentralWire.NOTIFY_MAX_BYTES))
    assertTrue(
      "max notify line must fit WIRE_MAX, was ${maxLine.length}",
      maxLine.length <= GattCentralWire.WIRE_MAX
    )
  }

  @Test
  fun wireRejectsPipeInjection() {
    try {
      GattCentralWire.linkEstablished("peer|evil")
      fail("pipe injection must reject")
    } catch (expected: IllegalArgumentException) {
      assertTrue(expected.message!!.contains("must not contain"))
    }
  }

  @Test
  fun writeStartSupportsAbsentMaximum() {
    assertEquals(
      "write.start|0|with-response|4|-|true|5000|1000",
      GattCentralWire.writeStart(0, "with-response", 4, null, true, 5000, 1000)
    )
  }

  @Test
  fun pathRegisterSupportsAbsentLevels() {
    assertEquals(
      "path.register|peer|180d|0|2a37|0|-|-|11|lease-a",
      GattCentralWire.pathRegister("peer", "180d", 0, "2a37", 0, null, null, 11, "lease-a")
    )
  }

  @Test
  fun postWithoutPermissionFailsClosedWithoutEnqueue() {
    val queued = ArrayDeque<String>()
    val bridge = UbmGattCentralBridge(
      enqueue = { wire -> queued.add(wire); queued.size },
      drain = { "" },
      hasBlePermissions = { false },
      worker = direct
    )
    val result = bridge.postEvent(GattCentralWire.linkEstablished("peer"))
    assertTrue(result is UbmGattCentralBridge.PostResult.PermissionDenied)
    assertEquals(
      "permission.denied|rn-android-boundary|postEvent|ble-permission-missing",
      (result as UbmGattCentralBridge.PostResult.PermissionDenied).identity
    )
    assertTrue("denied event must not reach the core queue", queued.isEmpty())
  }

  @Test
  fun postEnqueuesAndDrainsOnWorker() {
    val queued = ArrayDeque<String>()
    var drained = 0
    val seen = mutableListOf<GattObservation>()
    val bridge = UbmGattCentralBridge(
      enqueue = { wire -> queued.add(wire); queued.size },
      drain = {
        drained++
        queued.map { "{\"ok\":true,\"event\":\"probe\"}" }.joinToString("\n").also { queued.clear() }
      },
      hasBlePermissions = { true },
      onObservations = { seen.addAll(it) },
      worker = direct
    )
    val result = bridge.postEvent(GattCentralWire.expireSweep(1000))
    assertEquals(UbmGattCentralBridge.PostResult.Queued(1), result)
    assertEquals(1, drained)
    assertEquals(1, seen.size)
    assertTrue(seen[0].ok)
  }

  @Test
  fun enqueueFailureSurfacesWithoutThrowing() {
    val bridge = UbmGattCentralBridge(
      enqueue = { throw IllegalStateException("jni-detached") },
      drain = { "" },
      hasBlePermissions = { true },
      worker = direct
    )
    val result = bridge.postEvent(GattCentralWire.release())
    assertTrue(result is UbmGattCentralBridge.PostResult.EnqueueFailed)
    assertEquals(
      "jni-detached",
      (result as UbmGattCentralBridge.PostResult.EnqueueFailed).message
    )
  }

  @Test
  fun releaseOnDestroyEnqueuesReleaseAndStopsWorker() {
    val queued = ArrayDeque<String>()
    var drains = 0
    val bridge = UbmGattCentralBridge(
      enqueue = { wire -> queued.add(wire); queued.size },
      drain = {
        drains++
        "{\"ok\":true,\"event\":\"release\",\"state\":\"released\"}"
      },
      hasBlePermissions = { true },
      worker = direct
    )
    val observations = bridge.releaseOnDestroy()
    assertEquals(listOf("release"), queued.toList())
    assertEquals(1, drains)
    assertEquals("release", observations.single().event)
    // Worker stays stopped after destroy: later posts are refused outright
    // (never enqueued, no second drain, no misleading success).
    val refused = bridge.postEvent(GattCentralWire.expireSweep(2000))
    assertTrue(refused is UbmGattCentralBridge.PostResult.Shutdown)
    assertEquals(listOf("release"), queued.toList())
    assertEquals(1, drains)
  }

  @Test
  fun releaseOnDestroyProceedsWithoutPermission() {
    val queued = ArrayDeque<String>()
    val bridge = UbmGattCentralBridge(
      enqueue = { wire -> queued.add(wire); queued.size },
      drain = { "{\"ok\":true,\"event\":\"release\",\"state\":\"released\"}" },
      hasBlePermissions = { false },
      worker = direct
    )
    // Teardown is not a radio op: a missing permission must not skip it.
    val observations = bridge.releaseOnDestroy()
    assertEquals(listOf("release"), queued.toList())
    assertEquals("released", observations.single().let {
      assertTrue(it.ok); it.raw.substringAfter("\"state\":\"").substringBefore("\"")
    })
  }

  @Test
  fun shutdownIsIdempotentAndRefusesPosts() {
    var drains = 0
    val bridge = UbmGattCentralBridge(
      enqueue = { 1 },
      drain = { drains++; "" },
      hasBlePermissions = { true },
      worker = direct
    )
    bridge.shutdown()
    bridge.shutdown()
    assertTrue(bridge.postEvent(GattCentralWire.release()) is UbmGattCentralBridge.PostResult.Shutdown)
    assertEquals(0, drains)
  }

  @Test
  fun parseObservationsKeepsFailClosedIdentities() {
    val parsed = GattCentralWire.parseObservations(
      "{\"ok\":true,\"event\":\"scan.start\",\"op\":\"op-1\"}\n" +
        "{\"ok\":false,\"event\":\"read.start\",\"code\":\"gatt.stale-handle\"," +
        "\"domain\":\"gatt\",\"operation\":\"gatt-drain\",\"detail\":\"central-rejected\"}"
    )
    assertEquals(2, parsed.size)
    assertTrue(parsed[0].ok)
    assertEquals("scan.start", parsed[0].event)
    assertFalse(parsed[1].ok)
    assertEquals("gatt.stale-handle", parsed[1].code)
    assertEquals("gatt", parsed[1].domain)
    assertEquals("empty drain parses to no observations", 0,
      GattCentralWire.parseObservations("").size)
  }

  @Test
  fun parseObservationsDropsBlankLinesAndDecodesEscapes() {
    val parsed = GattCentralWire.parseObservations(
      "{\"ok\":true,\"event\":\"x\",\"detail\":\"a\\nB\\u0041\"}\n" +
        "{\"ok\":false,\"event\":\"y\",\"code\":\"c\"}\n" +
        "\n"
    )
    assertEquals("trailing/blank lines must not phantom", 2, parsed.size)
    assertTrue(parsed[0].ok)
    assertEquals("a\nBA", parsed[0].detail)
    assertFalse(parsed[1].ok)
  }

  @Test
  fun parseVerdictIsAnchoredNotSubstring() {
    val parsed = GattCentralWire.parseObservations(
      "{\"ok\":false,\"event\":\"z\",\"detail\":\"saw {\\\"ok\\\":true} inside\"}"
    )
    assertEquals(1, parsed.size)
    assertFalse("embedded ok:true must not flip the verdict", parsed[0].ok)
  }

  @Test
  fun f21ExecutorRejectionSurfacesInsteadOfFalseQueued() {
    // F21: when the worker executor rejects the drain task, the post
    // must report the scheduling failure — never Queued for work
    // nothing will drain. The line did reach the core queue (depth
    // is reported), but no drain was scheduled for it.
    val queued = ArrayDeque<String>()
    val rejecting = Executor { throw java.util.concurrent.RejectedExecutionException("injected") }
    val bridge = UbmGattCentralBridge(
      enqueue = { wire -> queued.add(wire); queued.size },
      drain = { "" },
      hasBlePermissions = { true },
      worker = rejecting
    )
    val result = bridge.postEvent(GattCentralWire.expireSweep(1000))
    assertTrue(
      "rejected schedule must surface, was $result",
      result is UbmGattCentralBridge.PostResult.ScheduleFailed
    )
    assertEquals(1, (result as UbmGattCentralBridge.PostResult.ScheduleFailed).depth)
    assertEquals(listOf(GattCentralWire.expireSweep(1000)), queued.toList())
    // A later post with a live executor still schedules: rejection is
    // per-post, and the stranded line drains with the next schedule.
    val seen = mutableListOf<GattObservation>()
    val revived = UbmGattCentralBridge(
      enqueue = { wire -> queued.add(wire); queued.size },
      drain = {
        queued.map { "{\"ok\":true,\"event\":\"recovered\"}" }.joinToString("\n").also { queued.clear() }
      },
      hasBlePermissions = { true },
      onObservations = { seen.addAll(it) },
      worker = direct
    )
    assertEquals(UbmGattCentralBridge.PostResult.Queued(2), revived.postEvent("second"))
    assertEquals(2, seen.size)
  }

  @Test
  fun f21PermissionProviderExceptionFailsClosedWithoutThrowing() {
    // F21: postEvent never throws — a throwing permission provider is a
    // classified fail-closed refusal, and nothing is enqueued.
    val queued = ArrayDeque<String>()
    val bridge = UbmGattCentralBridge(
      enqueue = { wire -> queued.add(wire); queued.size },
      drain = { "" },
      hasBlePermissions = { throw IllegalStateException("provider blew up") },
      worker = direct
    )
    val result = try {
      bridge.postEvent(GattCentralWire.linkEstablished("peer"))
    } catch (th: Throwable) {
      fail("postEvent must never throw, threw $th")
      return
    }
    assertTrue(result is UbmGattCentralBridge.PostResult.PermissionDenied)
    assertEquals(
      UbmGattCentralBridge.PERMISSION_PROVIDER_FAILED_IDENTITY,
      (result as UbmGattCentralBridge.PostResult.PermissionDenied).identity
    )
    assertTrue("refused event must not reach the core queue", queued.isEmpty())
  }

  @Test
  fun f21PostRacingDestroyIsNeverStrandedAsQueued() {
    // F21: a post that wins admission before destroy must have its line
    // drained (by the worker or the destroy path) — never acknowledged
    // as Queued and then stranded by a racing shutdown.
    val queued = ConcurrentLinkedQueue<String>()
    val seen = ConcurrentLinkedQueue<GattObservation>()
    val inEnqueue = CountDownLatch(1)
    val proceed = CountDownLatch(1)
    val calls = AtomicInteger(0)
    val threadPerTask = Executor { command -> Thread(command).start() }
    val bridge = UbmGattCentralBridge(
      enqueue = { wire ->
        if (calls.getAndIncrement() == 0) {
          inEnqueue.countDown()
          assertTrue("test gate released", proceed.await(10, TimeUnit.SECONDS))
        }
        queued.add(wire)
        queued.size
      },
      drain = {
        synchronized(queued) {
          queued.map { "{\"ok\":true,\"event\":\"$it\"}" }.joinToString("\n").also { queued.clear() }
        }
      },
      hasBlePermissions = { true },
      onObservations = { seen.addAll(it) },
      worker = threadPerTask
    )
    val releaser = Executors.newSingleThreadExecutor()
    try {
      val posted = CountDownLatch(1)
      var result: UbmGattCentralBridge.PostResult? = null
      Thread {
        result = bridge.postEvent("line-before-destroy")
        posted.countDown()
      }.start()
      assertTrue("post reached the native enqueue", inEnqueue.await(10, TimeUnit.SECONDS))
      // Destroy races the admitted post: the release path runs while the
      // post is still inside its enqueue call. Every drain reports
      // through onObservations, so `seen` alone is the delivery record
      // (the release return value would double-count its own drain).
      val released = releaser.submit<List<GattObservation>> { bridge.releaseOnDestroy() }
      Thread.sleep(300)
      proceed.countDown()
      assertTrue("post completed", posted.await(10, TimeUnit.SECONDS))
      released.get(10, TimeUnit.SECONDS)
      assertTrue(
        "admitted pre-destroy post must acknowledge Queued, was $result",
        result is UbmGattCentralBridge.PostResult.Queued
      )
      val delivered = seen.map { it.event }
      assertTrue(
        "admitted line must be drained exactly once, delivered=$delivered",
        delivered.count { it == "line-before-destroy" } == 1
      )
      assertTrue(
        "release line must be drained, delivered=$delivered",
        delivered.contains("release")
      )
      // After destroy, posts are refused outright — never queued.
      val sizeBefore = queued.size
      assertTrue(
        bridge.postEvent(GattCentralWire.expireSweep(1))
          is UbmGattCentralBridge.PostResult.Shutdown
      )
      assertEquals(sizeBefore, queued.size)
    } finally {
      releaser.shutdownNow()
    }
  }

  @Test
  fun f21ReleaseJoinsRunningDrainBeforeReturning() {
    // F21: releaseOnDestroy establishes worker completion — it must not
    // return (letting the owner close the native session) while a drain
    // is still running, and no observation may arrive after it returns.
    val queued = ConcurrentLinkedQueue<String>()
    val seen = ConcurrentLinkedQueue<GattObservation>()
    val drainEntered = CountDownLatch(1)
    val drainProceed = CountDownLatch(1)
    val drains = AtomicInteger(0)
    val worker = Executors.newSingleThreadExecutor()
    val bridge = UbmGattCentralBridge(
      enqueue = { wire -> queued.add(wire); queued.size },
      drain = {
        if (drains.getAndIncrement() == 0) {
          drainEntered.countDown()
          assertTrue("test gate released", drainProceed.await(10, TimeUnit.SECONDS))
        }
        synchronized(queued) {
          queued.map { "{\"ok\":true,\"event\":\"$it\"}" }.joinToString("\n").also { queued.clear() }
        }
      },
      hasBlePermissions = { true },
      onObservations = { seen.addAll(it) },
      worker = worker
    )
    try {
      val result = bridge.postEvent("line-during-drain")
      assertEquals(UbmGattCentralBridge.PostResult.Queued(1), result)
      assertTrue("worker drain started", drainEntered.await(10, TimeUnit.SECONDS))
      val releaser = Executors.newSingleThreadExecutor()
      try {
        val released = releaser.submit<List<GattObservation>> { bridge.releaseOnDestroy() }
        assertFalse(
          "release must wait for the running drain, not return under it",
          try {
            released.get(300, TimeUnit.MILLISECONDS)
            true
          } catch (_: java.util.concurrent.TimeoutException) {
            false
          }
        )
        drainProceed.countDown()
        released.get(10, TimeUnit.SECONDS)
        // `seen` alone is the delivery record: the release return value
        // would double-count the final drain's own observations.
        assertEquals(
          listOf("line-during-drain", "release"),
          seen.map { it.event }.sorted()
        )
        val countAtReturn = seen.size
        Thread.sleep(200)
        assertEquals(
          "no observation may arrive after the close barrier",
          countAtReturn,
          seen.size
        )
      } finally {
        releaser.shutdownNow()
      }
    } finally {
      worker.shutdownNow()
    }
  }

  @Test
  fun f01ParseSurfacesEffectsAndObservationsExactly() {
    // F01: kernel effects plus typed observations ride every drained line
    // and must reach the owner exactly (kinds, op binding, details with
    // escapes decoded) — never counted and dropped.
    val parsed = GattCentralWire.parseObservations(
      "{\"ok\":true,\"event\":\"scan.start\",\"op\":\"central-op-0\"," +
        "\"effects\":[{\"kind\":\"timer.schedule\",\"op\":\"central-op-0\"," +
        "\"detail\":\"deadline:6000\"}]," +
        "\"observations\":[{\"kind\":\"central.scan-start\",\"op\":\"central-op-0\"," +
        "\"detail\":\"scan.start\\nowner\"}]}"
    )
    assertEquals(1, parsed.size)
    assertTrue(parsed[0].ok)
    assertEquals(
      listOf(GattEffect("timer.schedule", "central-op-0", "deadline:6000")),
      parsed[0].effects
    )
    assertEquals(
      listOf(GattEffect("central.scan-start", "central-op-0", "scan.start\nowner")),
      parsed[0].observations
    )
  }

  @Test
  fun f01MissingSectionsParseAsEmptyForPreF01Lines() {
    val parsed = GattCentralWire.parseObservations("{\"ok\":true,\"event\":\"probe\"}")
    assertEquals(1, parsed.size)
    assertTrue(parsed[0].ok)
    assertTrue(parsed[0].effects.isEmpty())
    assertTrue(parsed[0].observations.isEmpty())
  }

  @Test
  fun f01EmptySectionsParseAsEmpty() {
    val parsed = GattCentralWire.parseObservations(
      "{\"ok\":true,\"event\":\"expire-sweep\",\"settled\":0,\"truncated\":false," +
        "\"effects\":[],\"observations\":[]}"
    )
    assertEquals(1, parsed.size)
    assertTrue(parsed[0].ok)
    assertTrue(parsed[0].effects.isEmpty())
    assertTrue(parsed[0].observations.isEmpty())
  }

  @Test
  fun f01MalformedEffectsFailTheLineClosed() {
    // A line whose effects section cannot be parsed fails closed (the host
    // must never mistake a truncated section for "no effects"); the raw
    // line is preserved for forensics.
    val raw =
      "{\"ok\":true,\"event\":\"scan.start\",\"effects\":[{\"kind\":\"timer.schedule\","
    val parsed = GattCentralWire.parseObservations(raw)
    assertEquals(1, parsed.size)
    assertFalse(parsed[0].ok)
    assertEquals("platform.failure", parsed[0].code)
    assertEquals(raw, parsed[0].raw)
  }

  @Test
  fun f01BridgeForwardsEffectsToTheOwner() {
    val seen = mutableListOf<GattObservation>()
    val bridge = UbmGattCentralBridge(
      enqueue = { 1 },
      drain = {
        "{\"ok\":true,\"event\":\"op.dispatch\",\"op\":\"central-op-3\"," +
          "\"effects\":[{\"kind\":\"radio.dispatch\",\"op\":\"central-op-3\"," +
          "\"detail\":\"radio.dispatch\"}," +
          "{\"kind\":\"state.publish\",\"op\":\"central-op-3\"," +
          "\"detail\":\"state.dispatched\"}],\"observations\":[]}"
      },
      hasBlePermissions = { true },
      onObservations = { seen.addAll(it) },
      worker = direct
    )
    bridge.postEvent(GattCentralWire.expireSweep(1000))
    assertEquals(1, seen.size)
    assertEquals(
      listOf(
        GattEffect("radio.dispatch", "central-op-3", "radio.dispatch"),
        GattEffect("state.publish", "central-op-3", "state.dispatched")
      ),
      seen[0].effects
    )
  }

  @Test
  fun wireBuildersRejectCommaAndNegativesFailFast() {
    try {
      GattCentralWire.scanStart("o", 1, 1, listOf("180d,180f"), "all", "none")
      fail("comma in serviceUuid must reject")
    } catch (expected: IllegalArgumentException) {
      assertTrue(expected.message!!.contains("must not contain ','"))
    }
    try {
      GattCentralWire.pathRegister("p", "180d", 0, "2a37", -1, null, null, 11, "l")
      fail("negative occurrence must reject")
    } catch (expected: IllegalArgumentException) {
      assertTrue(expected.message!!.contains("non-negative"))
    }
    try {
      GattCentralWire.writeStart(0, "m", 4, -5, true, 5000, 1000)
      fail("negative maximum must reject")
    } catch (expected: IllegalArgumentException) {
      assertTrue(expected.message!!.contains("non-negative"))
    }
  }
}
