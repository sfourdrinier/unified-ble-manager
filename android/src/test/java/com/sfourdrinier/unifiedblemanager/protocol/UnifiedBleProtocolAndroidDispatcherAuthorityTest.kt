// android/src/test/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolAndroidDispatcherAuthorityTest.kt

package com.sfourdrinier.unifiedblemanager.protocol

import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothDevice
import android.bluetooth.BluetoothGatt
import android.bluetooth.BluetoothGattCallback
import android.bluetooth.BluetoothManager
import android.bluetooth.BluetoothProfile
import android.bluetooth.le.BluetoothLeScanner
import android.bluetooth.le.ScanCallback
import android.bluetooth.le.ScanResult
import android.bluetooth.le.ScanSettings
import android.content.Context
import android.content.pm.PackageManager
import android.util.Log
import com.facebook.react.bridge.ReadableMap
import com.facebook.react.bridge.ReadableType
import com.sfourdrinier.unifiedblemanager.protocol.generated.RecordKind
import com.sfourdrinier.unifiedblemanager.radio.GattObservation
import com.sfourdrinier.unifiedblemanager.radio.UbmGattCoreBinding
import com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidGattRadio
import java.util.Collections
import java.util.concurrent.Executor
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicReference
import java.util.concurrent.atomic.AtomicBoolean
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test
import org.mockito.Mockito
import org.mockito.ArgumentMatchers.any
import org.mockito.ArgumentMatchers.eq

/**
 * R02 Android authority proofs (HOST-JVM unit tests, local Gradle runnable).
 *
 * The dispatcher executes the platform radio and consults the shared core
 * through `DeferredCoreShadow` over [UbmGattCoreBinding] (fake JNI here, real
 * wire builders + real bridge + real gate). The JSI boundary is stubbed with
 * a static mock; emitted RESULT bytes are parsed structurally and asserted
 * on their terminal outcome + error code.
 *
 * - core-rejection-binds: a core `ok:false` drain for the command's wire
 *   event must fail the command (`coreRejected`) even though the radio would
 *   succeed — never a radio success standing over a core refusal.
 * - shadow-unavailable-fails-loud: a missing/failed shadow at command time
 *   must surface an actionable `coreUnavailable` terminal — never silent
 *   radio-only execution.
 * - command-attestation: a healthy core shows the full round trip — the exact
 *   wire line enqueued, its drain observation consumed, and the radio success
 *   reported only with the core admitted.
 */
class UnifiedBleProtocolAndroidDispatcherAuthorityTest {

  private fun adapterLoss(dispatcher: UnifiedBleProtocolAndroidDispatcher) {
    val field = dispatcher.javaClass.getDeclaredField("radio")
    field.isAccessible = true
    (field.get(dispatcher) as OwnedAndroidGattRadio).onAdapterState?.invoke("Resetting")
  }

  @Test fun heldRejectedPlatformStartCannotReportSuccess() {
    org.junit.Assume.assumeTrue(System.getenv("UBM_HOST_CORE_JNI") == "1")
    val harness = Harness(FakeJni())
    val scanner = Mockito.mock(BluetoothLeScanner::class.java)
    val callbacks = mutableListOf<ScanCallback>()
    val entered = CountDownLatch(1)
    val release = CountDownLatch(1)
    val async = AtomicBoolean(false)
    val workerThread = AtomicReference<Thread>()
    val jni = object : UbmGattCoreBinding.CoreJni by UbmGattCoreBinding.RealCoreJni {
      override fun enqueue(handle: Long, wire: String): Int {
        if (wire.startsWith("scan.platform-started|")) {
          async.set(true)
          return UbmGattCoreBinding.RealCoreJni.enqueue(handle, "scan.platform-started|op-999999")
        }
        return UbmGattCoreBinding.RealCoreJni.enqueue(handle, wire)
      }
    }
    val worker = Executor { task ->
      if (async.getAndSet(false)) {
        val thread = Thread {
          Mockito.mockStatic(UnifiedBleProtocolJsiBinding::class.java).use { task.run() }
        }
        workerThread.set(thread)
        thread.start()
        check(entered.await(5, TimeUnit.SECONDS))
      } else task.run()
    }
    Mockito.`when`(harness.adapter.bluetoothLeScanner).thenReturn(scanner)
    Mockito.doAnswer { callbacks.add(it.getArgument(2)); null }
      .`when`(scanner).startScan(Mockito.isNull(), Mockito.any(ScanSettings::class.java), Mockito.any(ScanCallback::class.java))
    mockJsi(harness).use { Mockito.mockStatic(Log::class.java).use {
      Mockito.mockConstruction(ScanSettings.Builder::class.java) { builder, _ ->
        Mockito.`when`(builder.setScanMode(Mockito.anyInt())).thenReturn(builder)
        Mockito.`when`(builder.setCallbackType(Mockito.anyInt())).thenReturn(builder)
        Mockito.`when`(builder.setReportDelay(Mockito.anyLong())).thenReturn(builder)
        Mockito.`when`(builder.build()).thenReturn(Mockito.mock(ScanSettings::class.java))
      }.use {
        val dispatcher = harness.dispatcher { ctx, reject ->
          UbmGattCoreBinding(ctx, jni = jni, onCoreRejection = { observation ->
            if (observation.event == "scan.platform-started") { entered.countDown(); check(release.await(5, TimeUnit.SECONDS)) }
            reject(observation)
          }, worker = worker, clockMs = { 1000L })
        }
        try {
          dispatcher.dispatch(scanStartCommand(1, "held-platform-start"))
          val terminals = parseTerminals(harness.emitted.filter { parseRecord(it, 0).kindWire == RecordKind.RESULT.wireValue })
          assertEquals("failed", terminals.last().outcome)
          Mockito.verify(scanner).stopScan(callbacks.single())
        } finally { release.countDown(); workerThread.get()?.join(5000); dispatcher.close() }
      }
    } }
  }

  @Test fun resetDuringRadioStartCannotReportScanStarted() = resetDuringRadioStart(false)
  @Test fun concurrentResetDuringRadioStartCannotReportScanStarted() = resetDuringRadioStart(true)

  private fun resetDuringRadioStart(concurrent: Boolean) {
    org.junit.Assume.assumeTrue(System.getenv("UBM_HOST_CORE_JNI") == "1")
    val harness = Harness(FakeJni())
    val scanner = Mockito.mock(BluetoothLeScanner::class.java)
    val dispatcherRef = AtomicReference<UnifiedBleProtocolAndroidDispatcher>()
    val calls = mutableListOf<ScanCallback>()
    val resetThread = AtomicReference<Thread>()
    val resetFailure = AtomicReference<Throwable?>()
    Mockito.`when`(harness.adapter.bluetoothLeScanner).thenReturn(scanner)
    Mockito.doAnswer {
      calls.add(it.getArgument(2))
      if (calls.size == 1) {
        if (concurrent) {
          val thread = Thread {
            try {
              Mockito.mockStatic(UnifiedBleProtocolJsiBinding::class.java).use { adapterLoss(dispatcherRef.get()) }
            } catch (error: Throwable) { resetFailure.set(error) }
          }
          resetThread.set(thread)
          thread.start()
          val deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(5)
          while (thread.state != Thread.State.BLOCKED && System.nanoTime() < deadline) Thread.yield()
          assertEquals("reset must wait for the in-flight start", Thread.State.BLOCKED, thread.state)
        } else adapterLoss(dispatcherRef.get())
      }
      null
    }.`when`(scanner).startScan(Mockito.isNull(), Mockito.any(ScanSettings::class.java), Mockito.any(ScanCallback::class.java))
    mockJsi(harness).use { Mockito.mockStatic(Log::class.java).use {
      Mockito.mockConstruction(ScanSettings.Builder::class.java) { builder, _ ->
        Mockito.`when`(builder.setScanMode(Mockito.anyInt())).thenReturn(builder)
        Mockito.`when`(builder.setCallbackType(Mockito.anyInt())).thenReturn(builder)
        Mockito.`when`(builder.setReportDelay(Mockito.anyLong())).thenReturn(builder)
        Mockito.`when`(builder.build()).thenReturn(Mockito.mock(ScanSettings::class.java))
      }.use {
        val dispatcher = harness.dispatcher { ctx, reject ->
          UbmGattCoreBinding(ctx, jni = UbmGattCoreBinding.RealCoreJni, onCoreRejection = reject, worker = direct, clockMs = { 1000L })
        }
        dispatcherRef.set(dispatcher)
        try {
          dispatcher.dispatch(scanStartCommand(1, "reset-start"))
          resetThread.get()?.join(5000)
          assertTrue(resetThread.get()?.isAlive != true)
          resetFailure.get()?.let { throw AssertionError("reset callback failed", it) }
          val terminals = parseTerminals(harness.emitted.filter { parseRecord(it, 0).kindWire == RecordKind.RESULT.wireValue })
          assertEquals("failed", terminals.last().outcome)
          Mockito.verify(scanner).stopScan(calls[0])
          dispatcher.dispatch(scanStartCommand(2, "after-reset"))
          assertEquals(2, calls.size)
        } finally { resetThread.get()?.join(5000); dispatcher.close() }
      }
    } }
  }

  @Test fun failedResetEnqueueKeepsScanIdentityForCleanup() = failedReset(false)
  @Test fun queuedResetWithSchedulerFailureIsAttestedBeforeCleanup() = failedReset(true)
  @Test fun queuedResetWithFailedDrainIsNotEnqueuedTwice() = failedReset(true, true)

  @Test fun resetRetriesFailedNativeScanCleanupBeforeReplacement() {
    org.junit.Assume.assumeTrue(System.getenv("UBM_HOST_CORE_JNI") == "1")
    val harness = Harness(FakeJni())
    val scanner = Mockito.mock(BluetoothLeScanner::class.java)
    val callbacks = mutableListOf<ScanCallback>()
    val resets = java.util.concurrent.atomic.AtomicInteger()
    val jni = object : UbmGattCoreBinding.CoreJni by UbmGattCoreBinding.RealCoreJni {
      override fun enqueue(handle: Long, wire: String): Int {
        if (wire.startsWith("adapter.reset|")) resets.incrementAndGet()
        return UbmGattCoreBinding.RealCoreJni.enqueue(handle, wire)
      }
    }
    Mockito.`when`(harness.adapter.bluetoothLeScanner).thenReturn(scanner)
    Mockito.doAnswer { callbacks.add(it.getArgument(2)); null }
      .`when`(scanner).startScan(Mockito.isNull(), Mockito.any(ScanSettings::class.java), Mockito.any(ScanCallback::class.java))
    Mockito.doThrow(IllegalStateException("stop refused")).doNothing()
      .`when`(scanner).stopScan(Mockito.any(ScanCallback::class.java))
    mockJsi(harness).use { Mockito.mockStatic(Log::class.java).use {
      Mockito.mockConstruction(ScanSettings.Builder::class.java) { builder, _ ->
        Mockito.`when`(builder.setScanMode(Mockito.anyInt())).thenReturn(builder)
        Mockito.`when`(builder.setCallbackType(Mockito.anyInt())).thenReturn(builder)
        Mockito.`when`(builder.setReportDelay(Mockito.anyLong())).thenReturn(builder)
        Mockito.`when`(builder.build()).thenReturn(Mockito.mock(ScanSettings::class.java))
      }.use {
        val dispatcher = harness.dispatcher { ctx, reject ->
          UbmGattCoreBinding(ctx, jni = jni, onCoreRejection = reject, worker = direct, clockMs = { 1000L })
        }
        try {
          dispatcher.dispatch(scanStartCommand(1, "reset-cleanup-a"))
          adapterLoss(dispatcher)
          assertEquals("core reset must not wait for failed scan cleanup", 1, resets.get())
          harness.establish(dispatcher)
          dispatcher.dispatch(scanStartCommand(2, "reset-cleanup-b"))
          assertEquals("scan cleanup retry must not reset the new connection", 1, resets.get())
          dispatcher.dispatch(disconnectCommand(11, "disconnect-after-cleanup"))
          assertEquals(2, callbacks.size)
          Mockito.verify(scanner, Mockito.times(2)).stopScan(callbacks[0])
          val terminals = parseTerminals(harness.emitted.filter { parseRecord(it, 0).kindWire == RecordKind.RESULT.wireValue })
          assertTrue(terminals.any { it.resultKind == "scanStarted" && it.outcome == "succeeded" })
          assertEquals("succeeded", terminals.last().outcome)
        } finally { dispatcher.close() }
      }
    } }
  }

  @Test fun capturedStopSucceedsWhenResetSettlesItsExactScan() {
    org.junit.Assume.assumeTrue(System.getenv("UBM_HOST_CORE_JNI") == "1")
    val harness = Harness(FakeJni())
    val scanner = Mockito.mock(BluetoothLeScanner::class.java)
    val callbacks = mutableListOf<ScanCallback>()
    val failure = AtomicReference<Throwable>()
    Mockito.`when`(harness.adapter.bluetoothLeScanner).thenReturn(scanner)
    Mockito.doAnswer { callbacks.add(it.getArgument(2)); null }
      .`when`(scanner).startScan(Mockito.isNull(), Mockito.any(ScanSettings::class.java), Mockito.any(ScanCallback::class.java))
    mockJsi(harness).use { Mockito.mockStatic(Log::class.java).use {
      Mockito.mockConstruction(ScanSettings.Builder::class.java) { builder, _ ->
        Mockito.`when`(builder.setScanMode(Mockito.anyInt())).thenReturn(builder)
        Mockito.`when`(builder.setCallbackType(Mockito.anyInt())).thenReturn(builder)
        Mockito.`when`(builder.setReportDelay(Mockito.anyLong())).thenReturn(builder)
        Mockito.`when`(builder.build()).thenReturn(Mockito.mock(ScanSettings::class.java))
      }.use {
        val dispatcher = harness.dispatcher { ctx, reject ->
          UbmGattCoreBinding(ctx, jni = UbmGattCoreBinding.RealCoreJni, onCoreRejection = reject, worker = direct, clockMs = { 1000L })
        }
        val guard = dispatcher.javaClass.getDeclaredField("scanCoreSettlementGuard").apply { isAccessible = true }.get(dispatcher) ?: error("missing settlement guard")
        val stopper = Thread {
          try { mockJsi(harness).use { dispatcher.dispatch(stopScanCommand(2, "stop-reset-race")) } }
          catch (t: Throwable) { failure.set(t) }
        }
        try {
          dispatcher.dispatch(scanStartCommand(1, "stop-reset-start"))
          synchronized(guard) {
            stopper.start()
            val deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(5)
            while (stopper.state != Thread.State.BLOCKED && System.nanoTime() < deadline) Thread.yield()
            assertEquals(Thread.State.BLOCKED, stopper.state)
            adapterLoss(dispatcher)
          }
          stopper.join(5000)
          assertTrue(!stopper.isAlive)
          failure.get()?.let { throw AssertionError(it) }
          val terminals = parseTerminals(harness.emitted.filter { parseRecord(it, 0).kindWire == RecordKind.RESULT.wireValue })
          assertEquals("succeeded", terminals.last().outcome)
          Mockito.verify(scanner, Mockito.times(1)).stopScan(callbacks.single())
        } finally { stopper.join(5000); dispatcher.close() }
      }
    } }
  }

  @Test fun registryCancellationAfterStopCapture() = registryCancellationAfterStopCapture(false)
  @Test fun registryCancellationAfterStopCaptureDuringReset() = registryCancellationAfterStopCapture(true)

  private fun registryCancellationAfterStopCapture(reset: Boolean) {
    org.junit.Assume.assumeTrue(System.getenv("UBM_HOST_CORE_JNI") == "1")
    val executable = checkNotNull(System.getenv("UBM_DISPATCHER_REGISTRY_HELPER")) {
      "UBM_DISPATCHER_REGISTRY_HELPER is required when running host JNI registry tests"
    }
    JsiStaticStubs.Registry(executable).use { registry ->
      val harness = Harness(FakeJni())
      val scanner = Mockito.mock(BluetoothLeScanner::class.java)
      val callbacks = Collections.synchronizedList(mutableListOf<ScanCallback>())
      val failure = AtomicReference<Throwable?>()
      Mockito.`when`(harness.adapter.bluetoothLeScanner).thenReturn(scanner)
      Mockito.doAnswer { callbacks.add(it.getArgument(2)); null }
        .`when`(scanner).startScan(Mockito.isNull(), Mockito.any(ScanSettings::class.java), Mockito.any(ScanCallback::class.java))
      registryJsi(harness, registry).use { Mockito.mockStatic(Log::class.java).use {
        Mockito.mockConstruction(ScanSettings.Builder::class.java) { builder, _ ->
          Mockito.`when`(builder.setScanMode(Mockito.anyInt())).thenReturn(builder)
          Mockito.`when`(builder.setCallbackType(Mockito.anyInt())).thenReturn(builder)
          Mockito.`when`(builder.setReportDelay(Mockito.anyLong())).thenReturn(builder)
          Mockito.`when`(builder.build()).thenReturn(Mockito.mock(ScanSettings::class.java))
        }.use {
          val dispatcher = harness.dispatcher { ctx, reject ->
            UbmGattCoreBinding(ctx, jni = UbmGattCoreBinding.RealCoreJni, onCoreRejection = reject, worker = direct, clockMs = { 1000L })
          }
          val start = scanStartCommand(1, "rn-android-scan-1")
          val stop = stopScanCommand(2, "rn-android-operation-2")
          val cancel = commandBytes("cancel", 4, "rn-android-operation-4", mapOf(
            8 to ProtocolWireValue.RecordValue(correlationRecord(2, "rn-android-operation-2"))
          ))
          val guard = checkNotNull(dispatcher.javaClass.getDeclaredField("scanCoreSettlementGuard").apply { isAccessible = true }.get(dispatcher))
          val stopper = Thread {
            try { registryJsi(harness, registry).use { dispatcher.dispatch(stop) } }
            catch (error: Throwable) { failure.set(error) }
          }
          val canceller = Thread {
            try { registryJsi(harness, registry).use { dispatcher.dispatch(cancel) } }
            catch (error: Throwable) { failure.set(error) }
          }
          try {
            registry.register(start)
            dispatcher.dispatch(start)
            registry.register(stop)
            synchronized(guard) {
              stopper.start()
              val stopDeadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(5)
              while (stopper.state != Thread.State.BLOCKED && System.nanoTime() < stopDeadline) Thread.yield()
              assertEquals("stop must capture its target before cancellation", Thread.State.BLOCKED, stopper.state)
              registry.register(cancel)
              canceller.start()
              val cancelDeadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(5)
              while (harness.emitted.size < 2 && System.nanoTime() < cancelDeadline) Thread.yield()
              assertEquals("cancelled stop must settle in the registry", 2, harness.emitted.size)
              if (reset) adapterLoss(dispatcher)
            }
            stopper.join(5000); canceller.join(5000)
            assertTrue(!stopper.isAlive && !canceller.isAlive)
            failure.get()?.let { throw AssertionError("registry cancellation interleaving failed", it) }
            val terminals = parseTerminals(harness.emitted)
            assertEquals(1, terminals.count { it.resultKind == "cancelled" && it.outcome == "failed" })
            assertEquals(1, terminals.count { it.resultKind == "cancelled" && it.outcome == "succeeded" })
            assertEquals("alreadyTerminal", registry.cancel(2, "rn-android-operation-2"))
            val replacement = scanStartCommand(3, "rn-android-scan-3")
            registry.register(replacement)
            dispatcher.dispatch(replacement)
            assertEquals("replacement must reach the radio", 2, callbacks.size)
          } finally {
            stopper.join(5000); canceller.join(5000); dispatcher.close()
          }
        }
      } }
    }
  }

  @Test fun heldResetPublicationRetainsOwnershipUntilTheExactVerdict() {
    org.junit.Assume.assumeTrue(System.getenv("UBM_HOST_CORE_JNI") == "1")
    val harness = Harness(FakeJni())
    val scanner = Mockito.mock(BluetoothLeScanner::class.java)
    val callbacks = mutableListOf<ScanCallback>()
    val entered = CountDownLatch(1)
    val release = CountDownLatch(1)
    val async = AtomicBoolean(false)
    val resetCount = java.util.concurrent.atomic.AtomicInteger()
    val workerThread = AtomicReference<Thread>()
    val shadowRef = AtomicReference<UbmGattCoreBinding>()
    val jni = object : UbmGattCoreBinding.CoreJni by UbmGattCoreBinding.RealCoreJni {
      override fun enqueue(handle: Long, wire: String): Int {
        if (wire.startsWith("adapter.reset|")) {
          resetCount.incrementAndGet()
          UbmGattCoreBinding.RealCoreJni.enqueue(handle, "invalid")
          async.set(true)
        }
        return UbmGattCoreBinding.RealCoreJni.enqueue(handle, wire)
      }
    }
    val worker = Executor { task ->
      if (async.getAndSet(false)) {
        val thread = Thread { Mockito.mockStatic(UnifiedBleProtocolJsiBinding::class.java).use { task.run() } }
        workerThread.set(thread)
        thread.start()
        check(entered.await(5, TimeUnit.SECONDS))
      } else task.run()
    }
    Mockito.`when`(harness.adapter.bluetoothLeScanner).thenReturn(scanner)
    Mockito.doAnswer { callbacks.add(it.getArgument(2)); null }
      .`when`(scanner).startScan(Mockito.isNull(), Mockito.any(ScanSettings::class.java), Mockito.any(ScanCallback::class.java))
    mockJsi(harness).use { Mockito.mockStatic(Log::class.java).use {
      Mockito.mockConstruction(ScanSettings.Builder::class.java) { builder, _ ->
        Mockito.`when`(builder.setScanMode(Mockito.anyInt())).thenReturn(builder)
        Mockito.`when`(builder.setCallbackType(Mockito.anyInt())).thenReturn(builder)
        Mockito.`when`(builder.setReportDelay(Mockito.anyLong())).thenReturn(builder)
        Mockito.`when`(builder.build()).thenReturn(Mockito.mock(ScanSettings::class.java))
      }.use {
        val dispatcher = harness.dispatcher { ctx, reject ->
          UbmGattCoreBinding(ctx, jni = jni, onCoreRejection = { observation ->
            if (observation.event == "invalid") { entered.countDown(); check(release.await(5, TimeUnit.SECONDS)) }
            reject(observation)
          }, worker = worker, clockMs = { 1000L }).also { shadowRef.set(it) }
        }
        try {
          dispatcher.dispatch(scanStartCommand(1, "held-reset-a"))
          adapterLoss(dispatcher)
          dispatcher.dispatch(stopScanCommand(2, "held-reset-stop"))
          dispatcher.dispatch(scanStartCommand(3, "held-reset-blocked"))
          assertEquals(1, callbacks.size)
          assertEquals(1, resetCount.get())
          val shadow = shadowRef.get()
          assertTrue(shadow.postConnect("AA:BB:CC:DD:EE:FF", "between-resets") is com.sfourdrinier.unifiedblemanager.radio.UbmGattCentralBridge.PostResult.Queued)
          shadow.bridge.drainNow()
          assertTrue(shadow.takeConnectAdmission("AA:BB:CC:DD:EE:FF", "between-resets") is com.sfourdrinier.unifiedblemanager.radio.ConnectAdmissionResult.Admitted)
          adapterLoss(dispatcher) // A second physical loss after intervening core connection work.
          release.countDown()
          workerThread.get().join(5000)
          assertTrue(!workerThread.get().isAlive)
          dispatcher.dispatch(stopScanCommand(4, "held-reset-retry"))
          dispatcher.dispatch(scanStartCommand(5, "held-reset-b"))
          assertEquals(2, callbacks.size)
          assertEquals("each loss generation must be reset", 2, resetCount.get())
        } finally { release.countDown(); workerThread.get()?.join(5000); dispatcher.close() }
      }
    } }
  }

  private fun failedReset(schedule: Boolean, failDrain: Boolean = false) {
    org.junit.Assume.assumeTrue(System.getenv("UBM_HOST_CORE_JNI") == "1")
    val harness = Harness(FakeJni())
    val scanner = Mockito.mock(BluetoothLeScanner::class.java)
    val callbacks = mutableListOf<ScanCallback>()
    val first = AtomicBoolean(true)
    val rejectSchedule = AtomicBoolean(false)
    val rejectDrain = AtomicBoolean(false)
    val resetAttempts = java.util.concurrent.atomic.AtomicInteger()
    val jni = object : UbmGattCoreBinding.CoreJni by UbmGattCoreBinding.RealCoreJni {
      override fun enqueue(handle: Long, wire: String): Int {
        if (wire.startsWith("adapter.reset|") && first.compareAndSet(true, false)) {
          resetAttempts.incrementAndGet()
          if (!schedule) throw IllegalStateException("reset enqueue refused")
          rejectSchedule.set(true)
          rejectDrain.set(failDrain)
        } else if (wire.startsWith("adapter.reset|")) {
          resetAttempts.incrementAndGet()
        }
        return UbmGattCoreBinding.RealCoreJni.enqueue(handle, wire)
      }
      override fun drain(handle: Long): String {
        if (rejectDrain.getAndSet(false)) throw IllegalStateException("reset drain refused")
        return UbmGattCoreBinding.RealCoreJni.drain(handle)
      }
    }
    val worker = Executor { task ->
      if (rejectSchedule.getAndSet(false)) throw java.util.concurrent.RejectedExecutionException("reset scheduler refused")
      task.run()
    }
    Mockito.`when`(harness.adapter.bluetoothLeScanner).thenReturn(scanner)
    Mockito.doAnswer { callbacks.add(it.getArgument(2)); null }
      .`when`(scanner).startScan(Mockito.isNull(), Mockito.any(ScanSettings::class.java), Mockito.any(ScanCallback::class.java))
    mockJsi(harness).use { Mockito.mockStatic(Log::class.java).use {
      Mockito.mockConstruction(ScanSettings.Builder::class.java) { builder, _ ->
        Mockito.`when`(builder.setScanMode(Mockito.anyInt())).thenReturn(builder)
        Mockito.`when`(builder.setCallbackType(Mockito.anyInt())).thenReturn(builder)
        Mockito.`when`(builder.setReportDelay(Mockito.anyLong())).thenReturn(builder)
        Mockito.`when`(builder.build()).thenReturn(Mockito.mock(ScanSettings::class.java))
      }.use {
        val dispatcher = harness.dispatcher { ctx, reject ->
          UbmGattCoreBinding(ctx, jni = jni, onCoreRejection = reject, worker = worker, clockMs = { 1000L })
        }
        try {
          dispatcher.dispatch(scanStartCommand(1, "before-reset"))
          adapterLoss(dispatcher)
          dispatcher.dispatch(stopScanCommand(2, "cleanup"))
          dispatcher.dispatch(scanStartCommand(3, "after-reset"))
          assertEquals("reset must leave cleanup recoverable", 2, callbacks.size)
          val terminals = parseTerminals(harness.emitted.filter { parseRecord(it, 0).kindWire == RecordKind.RESULT.wireValue })
          assertEquals("scanStarted", terminals.last().resultKind)
          assertEquals(if (schedule) 1 else 2, resetAttempts.get())
        } finally { dispatcher.close() }
      }
    } }
  }

  @Test fun heldWorkerAdmissionDoesNotStartRadioWithoutVerdict() {
    org.junit.Assume.assumeTrue(System.getenv("UBM_HOST_CORE_JNI") == "1")
    val harness = Harness(FakeJni())
    val scanner = Mockito.mock(BluetoothLeScanner::class.java)
    val entered = CountDownLatch(1)
    val release = CountDownLatch(1)
    val workerThread = AtomicReference<Thread>()
    val held = AtomicBoolean(false)
    val callbacks = Collections.synchronizedList(mutableListOf<ScanCallback>())
    val jni = object : UbmGattCoreBinding.CoreJni by UbmGattCoreBinding.RealCoreJni {
      override fun enqueue(handle: Long, wire: String): Int {
        if (wire.startsWith("scan.start|") && held.compareAndSet(false, true)) {
          UbmGattCoreBinding.RealCoreJni.enqueue(handle, "invalid")
        }
        return UbmGattCoreBinding.RealCoreJni.enqueue(handle, wire)
      }
    }
    val worker = Executor { task ->
      val thread = Thread(task)
      workerThread.set(thread)
      thread.start()
      if (entered.count != 0L) assertTrue(entered.await(5, java.util.concurrent.TimeUnit.SECONDS))
    }
    Mockito.`when`(harness.adapter.bluetoothLeScanner).thenReturn(scanner)
    Mockito.doAnswer { callbacks.add(it.getArgument(2)); null }
      .`when`(scanner).startScan(Mockito.isNull(), Mockito.any(ScanSettings::class.java), Mockito.any(ScanCallback::class.java))
    mockJsi(harness).use { Mockito.mockStatic(Log::class.java).use {
      Mockito.mockConstruction(ScanSettings.Builder::class.java) { builder, _ ->
        Mockito.`when`(builder.setScanMode(Mockito.anyInt())).thenReturn(builder)
        Mockito.`when`(builder.setCallbackType(Mockito.anyInt())).thenReturn(builder)
        Mockito.`when`(builder.setReportDelay(Mockito.anyLong())).thenReturn(builder)
        Mockito.`when`(builder.build()).thenReturn(Mockito.mock(ScanSettings::class.java))
      }.use {
        val dispatcher = harness.dispatcher { ctx, reject ->
          UbmGattCoreBinding(ctx, jni = jni, onCoreRejection = { observation ->
            if (observation.event == "invalid") { entered.countDown(); release.await() }
            reject(observation)
          }, worker = worker, clockMs = { 1000L })
        }
        try {
          dispatcher.dispatch(scanStartCommand(1, "held-verdict"))
          assertEquals("radio must await the exact admission", 0, callbacks.size)
          dispatcher.dispatch(stopScanCommand(2, "pending-stop"))
          val pending = parseTerminals(harness.emitted.filter { parseRecord(it, 0).kindWire == RecordKind.RESULT.wireValue })
          assertEquals("unresolved ownership must not report a stop", "failed", pending.last().outcome)
          release.countDown()
          workerThread.get().join(5000)
          dispatcher.dispatch(stopScanCommand(3, "resolved-stop"))
          dispatcher.dispatch(scanStartCommand(4, "replacement"))
          assertEquals("resolved ownership must be retired", 1, callbacks.size)
        } finally { release.countDown(); workerThread.get()?.join(5000); dispatcher.close() }
      }
    } }
  }

  @Test fun rejectedQueuedAdmissionReleasesPendingClaim() {
    org.junit.Assume.assumeTrue(System.getenv("UBM_HOST_CORE_JNI") == "1")
    val harness = Harness(FakeJni())
    val scanner = Mockito.mock(BluetoothLeScanner::class.java)
    val failSchedule = AtomicBoolean(false)
    val failDrain = AtomicBoolean(false)
    val first = AtomicBoolean(true)
    val shadow = AtomicReference<UbmGattCoreBinding>()
    val callbacks = mutableListOf<ScanCallback>()
    val jni = object : UbmGattCoreBinding.CoreJni by UbmGattCoreBinding.RealCoreJni {
      override fun enqueue(handle: Long, wire: String): Int {
        if (wire.startsWith("scan.start|") && first.compareAndSet(true, false)) {
          UbmGattCoreBinding.RealCoreJni.enqueue(handle, "scan.start|external|100000|1000||all|none")
          UbmGattCoreBinding.RealCoreJni.drain(handle)
          failSchedule.set(true)
          failDrain.set(true)
        }
        return UbmGattCoreBinding.RealCoreJni.enqueue(handle, wire)
      }
      override fun drain(handle: Long): String {
        if (failDrain.getAndSet(false)) throw IllegalStateException("test drain")
        return UbmGattCoreBinding.RealCoreJni.drain(handle)
      }
    }
    val worker = Executor { task ->
      if (failSchedule.getAndSet(false)) throw java.util.concurrent.RejectedExecutionException("test scheduler")
      task.run()
    }
    Mockito.`when`(harness.adapter.bluetoothLeScanner).thenReturn(scanner)
    Mockito.doAnswer { callbacks.add(it.getArgument(2)); null }
      .`when`(scanner).startScan(Mockito.isNull(), Mockito.any(ScanSettings::class.java), Mockito.any(ScanCallback::class.java))
    mockJsi(harness).use { Mockito.mockStatic(Log::class.java).use {
      Mockito.mockConstruction(ScanSettings.Builder::class.java) { builder, _ ->
        Mockito.`when`(builder.setScanMode(Mockito.anyInt())).thenReturn(builder)
        Mockito.`when`(builder.setCallbackType(Mockito.anyInt())).thenReturn(builder)
        Mockito.`when`(builder.setReportDelay(Mockito.anyLong())).thenReturn(builder)
        Mockito.`when`(builder.build()).thenReturn(Mockito.mock(ScanSettings::class.java))
      }.use {
        val dispatcher = harness.dispatcher { ctx, reject ->
          UbmGattCoreBinding(ctx, jni = jni, onCoreRejection = reject, worker = worker, clockMs = { 1000L }).also { shadow.set(it) }
        }
        try {
          dispatcher.dispatch(scanStartCommand(1, "queued-rejected"))
          assertEquals(0, callbacks.size)
          dispatcher.dispatch(stopScanCommand(2, "cleanup-rejection"))
          val terminals = parseTerminals(harness.emitted.filter { parseRecord(it, 0).kindWire == RecordKind.RESULT.wireValue })
          assertEquals("definitive rejection must release the claim", "accepted", terminals.last().resultKind)
          assertEquals("definitive rejection must settle cleanup", "succeeded", terminals.last().outcome)
          shadow.get().postAdapterReset()
          dispatcher.dispatch(scanStartCommand(3, "replacement"))
          assertEquals(1, callbacks.size)
        } finally { dispatcher.close() }
      }
    } }
  }

  @Test fun scheduleFailedScanAdmissionIsRetiredBeforeReplacement() = scanScheduleFailure("scan.start|")
  @Test fun scheduleFailedPlatformStartIsRetiredBeforeReplacement() = scanScheduleFailure("scan.platform-started|")
  @Test fun scheduleFailedPlatformStartWithFailedDrainRemainsRetryable() = scanScheduleFailure("scan.platform-started|", true)
  @Test fun scheduleFailedScanSettlementIsNotPostedTwice() = scanScheduleFailure("scan.platform-event|")
  @Test fun scheduleFailedAdmissionWithFailedDrainRemainsRetryable() = scanScheduleFailure("scan.start|", true)
  @Test fun scheduleFailedSettlementWithFailedDrainIsNotPostedTwice() = scanScheduleFailure("scan.platform-event|", true)

  private fun scanScheduleFailure(failedPrefix: String, failDrain: Boolean = false) {
    org.junit.Assume.assumeTrue(System.getenv("UBM_HOST_CORE_JNI") == "1")
    val harness = Harness(FakeJni())
    val scanner = Mockito.mock(BluetoothLeScanner::class.java)
    val callbacks = mutableListOf<ScanCallback>()
    val reject = AtomicBoolean(false)
    val armed = AtomicBoolean(true)
    val rejectDrain = AtomicBoolean(false)
    val wires = mutableListOf<String>()
    val jni = object : UbmGattCoreBinding.CoreJni by UbmGattCoreBinding.RealCoreJni {
      override fun enqueue(handle: Long, wire: String): Int {
        wires.add(wire)
        val depth = UbmGattCoreBinding.RealCoreJni.enqueue(handle, wire)
        if (wire.startsWith(failedPrefix) && armed.compareAndSet(true, false)) {
          reject.set(true)
          rejectDrain.set(failDrain)
        }
        return depth
      }
      override fun drain(handle: Long): String {
        if (rejectDrain.getAndSet(false)) throw IllegalStateException("test drain")
        return UbmGattCoreBinding.RealCoreJni.drain(handle)
      }
    }
    val worker = Executor { task ->
      if (reject.getAndSet(false)) throw java.util.concurrent.RejectedExecutionException("test scheduler")
      task.run()
    }
    Mockito.`when`(harness.adapter.bluetoothLeScanner).thenReturn(scanner)
    Mockito.doAnswer { callbacks.add(it.getArgument(2)); null }
      .`when`(scanner).startScan(Mockito.isNull(), Mockito.any(ScanSettings::class.java), Mockito.any(ScanCallback::class.java))
    mockJsi(harness).use { Mockito.mockStatic(Log::class.java).use {
      Mockito.mockConstruction(ScanSettings.Builder::class.java) { builder, _ ->
        Mockito.`when`(builder.setScanMode(Mockito.anyInt())).thenReturn(builder)
        Mockito.`when`(builder.setCallbackType(Mockito.anyInt())).thenReturn(builder)
        Mockito.`when`(builder.setReportDelay(Mockito.anyLong())).thenReturn(builder)
        Mockito.`when`(builder.build()).thenReturn(Mockito.mock(ScanSettings::class.java))
      }.use {
        val dispatcher = harness.dispatcher { ctx, onRejection ->
          UbmGattCoreBinding(ctx, jni = jni, onCoreRejection = onRejection, worker = worker, clockMs = { 1000L })
        }
        try {
          dispatcher.dispatch(scanStartCommand(1, "schedule-a"))
          if (failedPrefix == "scan.start|") assertEquals(0, callbacks.size)
          dispatcher.dispatch(stopScanCommand(2, "schedule-stop"))
          dispatcher.dispatch(stopScanCommand(3, "schedule-retry"))
          dispatcher.dispatch(scanStartCommand(4, "schedule-b"))
          assertEquals(if (failedPrefix == "scan.start|") 1 else 2, callbacks.size)
          val terminals = parseTerminals(harness.emitted.filter { parseRecord(it, 0).kindWire == RecordKind.RESULT.wireValue })
          assertEquals("scanStarted", terminals.last().resultKind)
          assertEquals(1, wires.count { it.startsWith("scan.platform-event|") })
        } finally { dispatcher.close() }
      }
    } }
  }

  @Test fun permissionFailureRetainsCleanupDetailWithRealCore() {
    org.junit.Assume.assumeTrue(System.getenv("UBM_HOST_CORE_JNI") == "1")
    val harness = Harness(FakeJni())
    val scanner = Mockito.mock(BluetoothLeScanner::class.java)
    Mockito.`when`(harness.adapter.bluetoothLeScanner).thenReturn(scanner)
    Mockito.doThrow(SecurityException("scan permission refused")).doNothing()
      .`when`(scanner).startScan(Mockito.isNull(), Mockito.any(ScanSettings::class.java), Mockito.any(ScanCallback::class.java))
    Mockito.doThrow(IllegalStateException("cleanup refused")).doNothing()
      .`when`(scanner).stopScan(Mockito.any(ScanCallback::class.java))
    mockJsi(harness).use { Mockito.mockStatic(Log::class.java).use {
      Mockito.mockConstruction(ScanSettings.Builder::class.java) { builder, _ ->
        Mockito.`when`(builder.setScanMode(Mockito.anyInt())).thenReturn(builder)
        Mockito.`when`(builder.setCallbackType(Mockito.anyInt())).thenReturn(builder)
        Mockito.`when`(builder.setReportDelay(Mockito.anyLong())).thenReturn(builder)
        Mockito.`when`(builder.build()).thenReturn(Mockito.mock(ScanSettings::class.java))
      }.use {
        val dispatcher = harness.dispatcher { ctx, onRejection ->
          UbmGattCoreBinding(ctx, jni = UbmGattCoreBinding.RealCoreJni, onCoreRejection = onRejection, worker = direct, clockMs = { 1000L })
        }
        try {
          dispatcher.dispatch(scanStartCommand(1, "permission"))
          val terminal = parseTerminals(harness.emitted.filter { parseRecord(it, 0).kindWire == RecordKind.RESULT.wireValue }).last()
          assertEquals("permissionDenied", terminal.errorCode)
          assertTrue(terminal.errorMessage.contains("cleanup refused"))
          dispatcher.dispatch(stopScanCommand(2, "cleanup"))
          dispatcher.dispatch(scanStartCommand(3, "replacement"))
          val after = parseTerminals(harness.emitted.filter { parseRecord(it, 0).kindWire == RecordKind.RESULT.wireValue })
          assertEquals("scanStarted", after.last().resultKind)
        } finally { dispatcher.close() }
      }
    } }
  }

  @Test fun stopWaitsForHeldStartAdmission() {
    val harness = Harness(FakeJni())
    val scanner = Mockito.mock(BluetoothLeScanner::class.java)
    val entered = CountDownLatch(1)
    val release = CountDownLatch(1)
    val stopDone = CountDownLatch(1)
    val callback = AtomicReference<ScanCallback>()
    val failure = AtomicReference<Throwable>()
    Mockito.`when`(harness.adapter.bluetoothLeScanner).thenReturn(scanner)
    harness.jni.onScanStart = { entered.countDown(); release.await() }
    Mockito.doAnswer { callback.set(it.getArgument(2)); null }
      .`when`(scanner).startScan(Mockito.isNull(), Mockito.any(ScanSettings::class.java), Mockito.any(ScanCallback::class.java))
    val dispatcher = harness.dispatcher(harness.healthyFactory())
    val starter = Thread {
      try { mockJsi(harness).use {
        Mockito.mockConstruction(ScanSettings.Builder::class.java) { builder, _ ->
          Mockito.`when`(builder.setScanMode(Mockito.anyInt())).thenReturn(builder)
          Mockito.`when`(builder.setCallbackType(Mockito.anyInt())).thenReturn(builder)
          Mockito.`when`(builder.setReportDelay(Mockito.anyLong())).thenReturn(builder)
          Mockito.`when`(builder.build()).thenReturn(Mockito.mock(ScanSettings::class.java))
        }.use { dispatcher.dispatch(scanStartCommand(1, "held-start")) }
      } } catch (t: Throwable) { failure.set(t) }
    }
    val stopper = Thread {
      try { mockJsi(harness).use { dispatcher.dispatch(stopScanCommand(2, "held-stop")) } }
      catch (t: Throwable) { failure.set(t) } finally { stopDone.countDown() }
    }
    mockJsi(harness).use {
      try {
        starter.start(); entered.await(); stopper.start()
        while (stopper.state != Thread.State.BLOCKED && stopDone.count != 0L) Thread.yield()
        assertEquals("stop must wait for the admitted start", 1L, stopDone.count)
      } finally { release.countDown(); starter.join(); stopper.join() }
      try {
        failure.get()?.let { throw AssertionError(it) }
        Mockito.verify(scanner).stopScan(callback.get())
      } finally { dispatcher.close() }
    }
  }

  @Test fun cancelledFailedStopSettlesRealCoreBeforeReplacement() {
    org.junit.Assume.assumeTrue(System.getenv("UBM_HOST_CORE_JNI") == "1")
    val harness = Harness(FakeJni())
    val scanner = Mockito.mock(BluetoothLeScanner::class.java)
    val callbacks = mutableListOf<ScanCallback>()
    val cleanupEntered = CountDownLatch(1)
    val releaseCleanup = CountDownLatch(1)
    val failure = AtomicReference<Throwable>()
    Mockito.`when`(harness.adapter.bluetoothLeScanner).thenReturn(scanner)
    Mockito.doAnswer { callbacks.add(it.getArgument(2)); null }
      .`when`(scanner).startScan(Mockito.isNull(), Mockito.any(ScanSettings::class.java), Mockito.any(ScanCallback::class.java))
    Mockito.doThrow(IllegalStateException("first stop failed")).doNothing()
      .`when`(scanner).stopScan(Mockito.any(ScanCallback::class.java))
    mockJsi(harness).use {
      Mockito.mockStatic(Log::class.java).use {
        Mockito.mockConstruction(ScanSettings.Builder::class.java) { builder, _ ->
          Mockito.`when`(builder.setScanMode(Mockito.anyInt())).thenReturn(builder)
          Mockito.`when`(builder.setCallbackType(Mockito.anyInt())).thenReturn(builder)
          Mockito.`when`(builder.setReportDelay(Mockito.anyLong())).thenReturn(builder)
          Mockito.`when`(builder.build()).thenReturn(Mockito.mock(ScanSettings::class.java))
        }.use {
          val dispatcher = harness.dispatcher { ctx, reject ->
            UbmGattCoreBinding(ctx, jni = UbmGattCoreBinding.RealCoreJni, onCoreRejection = reject, worker = direct, clockMs = { 1000L })
          }
          val radioField = dispatcher.javaClass.getDeclaredField("radio").apply { isAccessible = true }
          val radio = radioField.get(dispatcher) as com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidGattRadio
          radio.onCleanupFailure = { cleanupEntered.countDown(); releaseCleanup.await() }
          val stopper = Thread {
            try { mockJsi(harness).use { Mockito.mockStatic(Log::class.java).use { dispatcher.dispatch(stopScanCommand(2, "racing-stop")) } } }
            catch (t: Throwable) { failure.set(t) }
          }
          try {
            dispatcher.dispatch(scanStartCommand(1, "real-start"))
            stopper.start(); cleanupEntered.await()
            dispatcher.cancelPendingOperation(2, "racing-stop")
            releaseCleanup.countDown(); stopper.join()
            failure.get()?.let { throw AssertionError(it) }
            dispatcher.dispatch(scanStartCommand(3, "replacement"))
            assertEquals("real core must admit replacement", 2, callbacks.size)
          } finally { releaseCleanup.countDown(); stopper.join(); dispatcher.close() }
        }
      }
    }
  }

  @Test fun startAndCompensationFailureIsReportedAndRetried() = startAndCompensationFailure(false)
  @Test fun startAndCompensationFailureIsRetriedWithRealCore() = startAndCompensationFailure(true)

  private fun startAndCompensationFailure(realCore: Boolean) {
    if (realCore) org.junit.Assume.assumeTrue(System.getenv("UBM_HOST_CORE_JNI") == "1")
    val harness = Harness(FakeJni())
    val scanner = Mockito.mock(BluetoothLeScanner::class.java)
    Mockito.`when`(harness.adapter.bluetoothLeScanner).thenReturn(scanner)
    Mockito.doThrow(IllegalStateException("start refused")).doNothing()
      .`when`(scanner).startScan(Mockito.isNull(), Mockito.any(ScanSettings::class.java), Mockito.any(ScanCallback::class.java))
    Mockito.doThrow(IllegalStateException("cleanup refused")).doNothing()
      .`when`(scanner).stopScan(Mockito.any(ScanCallback::class.java))
    mockJsi(harness).use { Mockito.mockStatic(Log::class.java).use {
      Mockito.mockConstruction(ScanSettings.Builder::class.java) { builder, _ ->
        Mockito.`when`(builder.setScanMode(Mockito.anyInt())).thenReturn(builder)
        Mockito.`when`(builder.setCallbackType(Mockito.anyInt())).thenReturn(builder)
        Mockito.`when`(builder.setReportDelay(Mockito.anyLong())).thenReturn(builder)
        Mockito.`when`(builder.build()).thenReturn(Mockito.mock(ScanSettings::class.java))
      }.use {
        val dispatcher = harness.dispatcher(if (realCore) { ctx, reject ->
          UbmGattCoreBinding(ctx, jni = UbmGattCoreBinding.RealCoreJni, onCoreRejection = reject, worker = direct, clockMs = { 1000L })
        } else harness.healthyFactory())
        try {
          dispatcher.dispatch(scanStartCommand(1, "start-failure"))
          val terminal = parseTerminals(harness.emitted.filter { parseRecord(it, 0).kindWire == RecordKind.RESULT.wireValue }).last()
          assertEquals("radioFailure", terminal.errorCode)
          assertTrue(terminal.errorMessage.contains("start refused") && terminal.errorMessage.contains("cleanup refused"))
          dispatcher.dispatch(stopScanCommand(2, "retry-cleanup"))
          dispatcher.dispatch(scanStartCommand(3, "replacement"))
          val after = parseTerminals(harness.emitted.filter { parseRecord(it, 0).kindWire == RecordKind.RESULT.wireValue })
          assertEquals("scanStarted", after.last().resultKind)
        } finally { dispatcher.close() }
      }
    } }
  }

  @Test fun failedScanRetiresRealCoreBeforeReplacement() = realCoreReplacement(true)
  @Test fun stoppedScanRetiresRealCoreBeforeReplacement() = realCoreReplacement(false)
  @Test fun failedNativeStopRemainsRetryableThroughRealCore() = realCoreReplacement(false, true)
  @Test fun failedSourceCleanupRemainsRetryableThroughRealCore() = realCoreReplacement(true, true)

  private fun realCoreReplacement(sourceFailed: Boolean, failFirstStop: Boolean = false) {
    org.junit.Assume.assumeTrue("Set UBM_HOST_CORE_JNI=1 with a host-built JNI library on java.library.path", System.getenv("UBM_HOST_CORE_JNI") == "1")
    val harness = Harness(FakeJni())
    val scanner = Mockito.mock(BluetoothLeScanner::class.java)
    val callbacks = mutableListOf<ScanCallback>()
    Mockito.`when`(harness.adapter.bluetoothLeScanner).thenReturn(scanner)
    Mockito.doAnswer { callbacks.add(it.getArgument(2)); null }
      .`when`(scanner).startScan(Mockito.isNull(), Mockito.any(ScanSettings::class.java), Mockito.any(ScanCallback::class.java))
    if (failFirstStop) {
      Mockito.doThrow(IllegalStateException("stop failed")).doNothing()
        .`when`(scanner).stopScan(Mockito.any(ScanCallback::class.java))
    }
    mockJsi(harness).use {
      Mockito.mockStatic(Log::class.java).use {
        Mockito.mockConstruction(ScanSettings.Builder::class.java) { builder, _ ->
          Mockito.`when`(builder.setScanMode(Mockito.anyInt())).thenReturn(builder)
          Mockito.`when`(builder.setCallbackType(Mockito.anyInt())).thenReturn(builder)
          Mockito.`when`(builder.setReportDelay(Mockito.anyLong())).thenReturn(builder)
          Mockito.`when`(builder.build()).thenReturn(Mockito.mock(ScanSettings::class.java))
        }.use {
          val dispatcher = harness.dispatcher { ctx, reject ->
            UbmGattCoreBinding(ctx, jni = UbmGattCoreBinding.RealCoreJni, onCoreRejection = reject, worker = direct, clockMs = { 1000L })
          }
          try {
            dispatcher.dispatch(scanStartCommand(1, "real-a"))
            assertEquals(1, callbacks.size)
            if (sourceFailed) callbacks.single().onScanFailed(7)
            else dispatcher.dispatch(stopScanCommand(3, "real-stop-a"))
            if (failFirstStop) dispatcher.dispatch(stopScanCommand(4, "real-stop-retry"))
            dispatcher.dispatch(scanStartCommand(2, "real-b"))
            assertEquals("replacement must be admitted by the real core", 2, callbacks.size)
            val terminals = parseTerminals(harness.emitted.filter { parseRecord(it, 0).kindWire == RecordKind.RESULT.wireValue })
            assertEquals("replacement was rejected: $terminals", 2, terminals.count { it.resultKind == "scanStarted" && it.outcome == "succeeded" })
            dispatcher.dispatch(stopScanCommand(5, "real-stop-b"))
            dispatcher.dispatch(scanStartCommand(6, "real-c"))
            assertEquals("replacement must itself retire cleanly", 3, callbacks.size)
          } finally { dispatcher.close() }
        }
      }
    }
  }

  @Test fun missingNativeIdentityProbeFailsClosed() {
    assertTrue(!UnifiedBleProtocolJsiBinding.scanEventIdentityAvailable())
  }

  @Test fun oldOrMalformedJsPeerCannotNegotiateCorrelatedScanEvents() {
    val request = Mockito.mock(ReadableMap::class.java)
    val key = "scanEventIdentitySupported"
    fun rejected() {
      try { UnifiedBleProtocolJsiBinding.requireScanEventIdentitySupport(request); fail("peer must be rejected") }
      catch (_: IllegalArgumentException) { }
    }
    rejected()
    Mockito.`when`(request.hasKey(key)).thenReturn(true)
    Mockito.`when`(request.getType(key)).thenReturn(ReadableType.String)
    rejected()
    Mockito.`when`(request.getType(key)).thenReturn(ReadableType.Boolean)
    rejected()
    Mockito.`when`(request.getBoolean(key)).thenReturn(true)
    rejected() // The old scan-identity peer cannot speak registry admission lanes.
    val admissionKey = "androidRegistryAdmissionSupported"
    Mockito.`when`(request.hasKey(admissionKey)).thenReturn(true)
    Mockito.`when`(request.isNull(admissionKey)).thenReturn(false)
    Mockito.`when`(request.getType(admissionKey)).thenReturn(ReadableType.Boolean)
    rejected()
    Mockito.`when`(request.getBoolean(admissionKey)).thenReturn(true)
    UnifiedBleProtocolJsiBinding.requireScanEventIdentitySupport(request)
  }

  @Test fun cancellationBeforeRadioRegistrationDoesNotOrphanScan() = heldStartCancellation(false)
  @Test fun cancellationDuringRadioStartDoesNotOrphanScan() = heldStartCancellation(true)
  @Test fun realCoreCancellationDuringRadioStartAllowsReplacement() = heldStartCancellation(true, true)

  private fun heldStartCancellation(holdRadio: Boolean, realCore: Boolean = false) {
    if (realCore) org.junit.Assume.assumeTrue(System.getenv("UBM_HOST_CORE_JNI") == "1")
    val harness = Harness(FakeJni())
    val scanner = Mockito.mock(BluetoothLeScanner::class.java)
    val entered = CountDownLatch(1)
    val release = CountDownLatch(1)
    val cancelled = CountDownLatch(1)
    val callback = AtomicReference<ScanCallback?>()
    val failure = AtomicReference<Throwable?>()
    Mockito.`when`(harness.adapter.bluetoothLeScanner).thenReturn(scanner)
    if (!holdRadio) harness.jni.onScanStart = { entered.countDown(); check(release.await(5, TimeUnit.SECONDS)) }
    Mockito.doAnswer {
      callback.set(it.getArgument(2))
      if (holdRadio) { entered.countDown(); check(release.await(5, TimeUnit.SECONDS)) }
      null
    }.`when`(scanner).startScan(Mockito.isNull(), Mockito.any(ScanSettings::class.java), Mockito.any(ScanCallback::class.java))
    val dispatcher = harness.dispatcher(if (realCore) { ctx, reject ->
      UbmGattCoreBinding(ctx, jni = UbmGattCoreBinding.RealCoreJni, onCoreRejection = reject, worker = direct, clockMs = { 1000L })
    } else harness.healthyFactory())
    val starter = Thread {
      try {
        mockJsi(harness).use {
          Mockito.mockConstruction(ScanSettings.Builder::class.java) { builder, _ ->
            Mockito.`when`(builder.setScanMode(Mockito.anyInt())).thenReturn(builder)
            Mockito.`when`(builder.setCallbackType(Mockito.anyInt())).thenReturn(builder)
            Mockito.`when`(builder.setReportDelay(Mockito.anyLong())).thenReturn(builder)
            Mockito.`when`(builder.build()).thenReturn(Mockito.mock(ScanSettings::class.java))
          }.use { dispatcher.dispatch(scanStartCommand(1, "cancel-start")) }
        }
      } catch (error: Throwable) { failure.set(error) }
    }
    val canceller = Thread {
      try { mockJsi(harness).use { dispatcher.cancelPendingOperation(1, "cancel-start") } }
      catch (error: Throwable) { failure.set(error) }
      finally { cancelled.countDown() }
    }
    mockJsi(harness).use {
      try {
        starter.start()
        assertTrue(entered.await(5, TimeUnit.SECONDS))
        canceller.start()
        val deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(5)
        while (canceller.state != Thread.State.BLOCKED && cancelled.count != 0L && System.nanoTime() < deadline) Thread.yield()
        release.countDown()
        starter.join(5000); canceller.join(5000)
        assertTrue(!starter.isAlive && !canceller.isAlive)
        failure.get()?.let { throw AssertionError("scan race failed", it) }
        callback.get()?.let { Mockito.verify(scanner).stopScan(it) }
        val terminals = parseTerminals(harness.emitted.filter { parseRecord(it, 0).kindWire == RecordKind.RESULT.wireValue })
        assertEquals("cancelled start must not succeed: $terminals", "cancelled", terminals.single().resultKind)
        assertEquals("cancelled", terminals.single().cause)
        if (realCore) {
          Mockito.mockConstruction(ScanSettings.Builder::class.java) { builder, _ ->
            Mockito.`when`(builder.setScanMode(Mockito.anyInt())).thenReturn(builder)
            Mockito.`when`(builder.setCallbackType(Mockito.anyInt())).thenReturn(builder)
            Mockito.`when`(builder.setReportDelay(Mockito.anyLong())).thenReturn(builder)
            Mockito.`when`(builder.build()).thenReturn(Mockito.mock(ScanSettings::class.java))
          }.use { dispatcher.dispatch(scanStartCommand(2, "after-cancel")) }
          val after = parseTerminals(harness.emitted.filter { parseRecord(it, 0).kindWire == RecordKind.RESULT.wireValue })
          assertEquals("succeeded", after.last().outcome)
          assertEquals("scanStarted", after.last().resultKind)
        }
      } finally { release.countDown(); starter.join(5000); canceller.join(5000); dispatcher.close() }
    }
  }

  @Test fun oldOrdinaryStopDoesNotClearReplacementCommand() {
    val harness = Harness(FakeJni())
    val scanner = Mockito.mock(BluetoothLeScanner::class.java)
    val peer = Mockito.mock(BluetoothDevice::class.java)
    val result = Mockito.mock(ScanResult::class.java)
    val callbacks = mutableListOf<ScanCallback>()
    val workerFailure = AtomicReference<Throwable?>()
    Mockito.`when`(harness.adapter.bluetoothLeScanner).thenReturn(scanner)
    Mockito.`when`(result.device).thenReturn(peer)
    Mockito.`when`(peer.address).thenReturn("AA:BB:CC:DD:EE:01")
    Mockito.doAnswer { callbacks.add(it.getArgument(2)); null }
      .`when`(scanner).startScan(Mockito.isNull(), Mockito.any(ScanSettings::class.java), Mockito.any(ScanCallback::class.java))
    mockJsi(harness).use { jsi ->
      Mockito.mockStatic(Log::class.java).use {
        Mockito.mockConstruction(ScanSettings.Builder::class.java) { builder, _ ->
          Mockito.`when`(builder.setScanMode(Mockito.anyInt())).thenReturn(builder)
          Mockito.`when`(builder.setCallbackType(Mockito.anyInt())).thenReturn(builder)
          Mockito.`when`(builder.setReportDelay(Mockito.anyLong())).thenReturn(builder)
          Mockito.`when`(builder.build()).thenReturn(Mockito.mock(ScanSettings::class.java))
        }.use {
          val dispatcher = harness.dispatcher(harness.healthyFactory())
          try {
            dispatcher.dispatch(scanStartCommand(1, "a"))
            val radioField = dispatcher.javaClass.getDeclaredField("radio").apply { isAccessible = true }
            val radio = radioField.get(dispatcher)
            val sessionsField = radio.javaClass.getDeclaredField("scanSessions").apply { isAccessible = true }
            val sessions = sessionsField.get(radio) as Map<*, *>
            val session = sessions.values.single()!!
            val worker = Thread {
              try {
                Mockito.mockStatic(UnifiedBleProtocolJsiBinding::class.java).use {
                  dispatcher.dispatch(stopScanCommand(2, "stop-a"))
                }
              } catch (error: Throwable) { workerFailure.set(error) }
            }
            synchronized(session) {
              worker.start()
              val deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(5)
              while (worker.state != Thread.State.BLOCKED && System.nanoTime() < deadline) Thread.yield()
              assertEquals(Thread.State.BLOCKED, worker.state)
              callbacks[0].onScanFailed(7)
              dispatcher.dispatch(scanStartCommand(3, "b"))
              assertEquals(2, callbacks.size)
            }
            worker.join(5000)
            assertTrue(!worker.isAlive)
            workerFailure.get()?.let { throw AssertionError("stop failed", it) }
            callbacks[1].onScanResult(ScanSettings.CALLBACK_TYPE_ALL_MATCHES, result)
            jsi.verify { UnifiedBleProtocolJsiBinding.emitAdvertisement(Mockito.eq(NATIVE_HANDLE), Mockito.any(), Mockito.eq("AA:BB:CC:DD:EE:01"), Mockito.any(), Mockito.anyInt(), Mockito.anyInt(), Mockito.anyBoolean(), Mockito.anyInt(), Mockito.anyLong(), Mockito.anyBoolean(), Mockito.any(), Mockito.any(), Mockito.any(), Mockito.any(), Mockito.any(), Mockito.any(), Mockito.any()) }
            Mockito.verify(scanner, Mockito.never()).stopScan(callbacks[1])
            dispatcher.dispatch(stopScanCommand(4, "stop-b"))
            Mockito.verify(scanner).stopScan(callbacks[1])
          } finally { dispatcher.close() }
        }
      }
    }
  }

  @Test fun delayedHistoricalAdvertisementCannotReachReplacement() = delayedHistoricalScanEvent(false)
  @Test fun delayedHistoricalFailureCannotStopReplacement() = delayedHistoricalScanEvent(true)

  @Test fun repeatedFailureAfterFailedCleanupRetainsItsOriginAcrossReplacement() {
    val harness = Harness(FakeJni())
    val scanner = Mockito.mock(BluetoothLeScanner::class.java)
    val callbacks = mutableListOf<ScanCallback>()
    val entered = CountDownLatch(1)
    val release = CountDownLatch(1)
    val correlation = AtomicReference<ByteArray>()
    val workerFailure = AtomicReference<Throwable?>()
    Mockito.`when`(harness.adapter.bluetoothLeScanner).thenReturn(scanner)
    Mockito.doAnswer { callbacks.add(it.getArgument(2)); null }
      .`when`(scanner).startScan(Mockito.isNull(), Mockito.any(ScanSettings::class.java), Mockito.any(ScanCallback::class.java))
    Mockito.doThrow(IllegalStateException("first cleanup failed")).doNothing()
      .`when`(scanner).stopScan(Mockito.any(ScanCallback::class.java))
    mockJsi(harness).use {
      Mockito.mockConstruction(ScanSettings.Builder::class.java) { builder, _ ->
        Mockito.`when`(builder.setScanMode(Mockito.anyInt())).thenReturn(builder)
        Mockito.`when`(builder.setCallbackType(Mockito.anyInt())).thenReturn(builder)
        Mockito.`when`(builder.setReportDelay(Mockito.anyLong())).thenReturn(builder)
        Mockito.`when`(builder.build()).thenReturn(Mockito.mock(ScanSettings::class.java))
      }.use {
        val dispatcher = harness.dispatcher(harness.healthyFactory())
        val worker = Thread {
          try {
            Mockito.mockStatic(UnifiedBleProtocolJsiBinding::class.java).use { jsi ->
              JsiStaticStubs.holdScanFailure(jsi, entered, release, correlation)
              Mockito.mockStatic(Log::class.java).use { callbacks[0].onScanFailed(7) }
            }
          } catch (error: Throwable) { workerFailure.set(error) }
        }
        try {
          val commandA = scanStartCommand(1, "a")
          dispatcher.dispatch(commandA)
          Mockito.mockStatic(Log::class.java).use { callbacks[0].onScanFailed(6) }
          worker.start()
          assertTrue(entered.await(5, TimeUnit.SECONDS))
          Mockito.verify(scanner, Mockito.times(2)).stopScan(callbacks[0])
          dispatcher.dispatch(scanStartCommand(3, "b"))
          assertEquals(2, callbacks.size)
          release.countDown()
          worker.join(5000)
          assertTrue(!worker.isAlive)
          workerFailure.get()?.let { throw AssertionError("failure callback failed", it) }
          val expected = ProtocolWireEncoder.encode(ProtocolCommandDecoder.decodeCommand(commandA).requiredRecord(2))
          assertTrue(expected.contentEquals(correlation.get()))
          Mockito.verify(scanner, Mockito.never()).stopScan(callbacks[1])
          dispatcher.dispatch(stopScanCommand(4, "stop-b"))
          Mockito.verify(scanner).stopScan(callbacks[1])
        } finally { release.countDown(); worker.join(5000); dispatcher.close() }
      }
    }
  }

  @Test fun advertisementHeldAtNativeEmissionRetainsItsOriginAcrossReplacement() {
    val harness = Harness(FakeJni())
    val scanner = Mockito.mock(BluetoothLeScanner::class.java)
    val peer = Mockito.mock(BluetoothDevice::class.java)
    val result = Mockito.mock(ScanResult::class.java)
    val callbacks = mutableListOf<ScanCallback>()
    val entered = CountDownLatch(1)
    val release = CountDownLatch(1)
    val correlation = AtomicReference<ByteArray>()
    val workerFailure = AtomicReference<Throwable?>()
    Mockito.`when`(harness.adapter.bluetoothLeScanner).thenReturn(scanner)
    Mockito.`when`(result.device).thenReturn(peer)
    Mockito.`when`(peer.address).thenReturn("AA:BB:CC:DD:EE:01")
    Mockito.doAnswer { callbacks.add(it.getArgument(2)); null }
      .`when`(scanner).startScan(Mockito.isNull(), Mockito.any(ScanSettings::class.java), Mockito.any(ScanCallback::class.java))
    mockJsi(harness).use {
      Mockito.mockConstruction(ScanSettings.Builder::class.java) { builder, _ ->
        Mockito.`when`(builder.setScanMode(Mockito.anyInt())).thenReturn(builder)
        Mockito.`when`(builder.setCallbackType(Mockito.anyInt())).thenReturn(builder)
        Mockito.`when`(builder.setReportDelay(Mockito.anyLong())).thenReturn(builder)
        Mockito.`when`(builder.build()).thenReturn(Mockito.mock(ScanSettings::class.java))
      }.use {
        val dispatcher = harness.dispatcher(harness.healthyFactory())
        val worker = Thread {
          try {
            Mockito.mockStatic(UnifiedBleProtocolJsiBinding::class.java).use { jsi ->
              JsiStaticStubs.holdAdvertisement(jsi, entered, release, correlation)
              callbacks[0].onScanResult(ScanSettings.CALLBACK_TYPE_ALL_MATCHES, result)
            }
          } catch (error: Throwable) { workerFailure.set(error) }
        }
        try {
          val commandA = scanStartCommand(1, "a")
          dispatcher.dispatch(commandA)
          worker.start()
          assertTrue(entered.await(5, TimeUnit.SECONDS))
          dispatcher.dispatch(stopScanCommand(2, "stop-a"))
          dispatcher.dispatch(scanStartCommand(3, "b"))
          assertEquals(2, callbacks.size)
          release.countDown()
          worker.join(5000)
          assertTrue(!worker.isAlive)
          workerFailure.get()?.let { throw AssertionError("advertisement callback failed", it) }
          val expected = ProtocolWireEncoder.encode(ProtocolCommandDecoder.decodeCommand(commandA).requiredRecord(2))
          assertTrue(expected.contentEquals(correlation.get()))
          Mockito.verify(scanner, Mockito.never()).stopScan(callbacks[1])
          dispatcher.dispatch(stopScanCommand(4, "stop-b"))
          Mockito.verify(scanner).stopScan(callbacks[1])
        } finally { release.countDown(); worker.join(5000); dispatcher.close() }
      }
    }
  }

  private fun delayedHistoricalScanEvent(failure: Boolean) {
    val harness = Harness(FakeJni())
    val scanner = Mockito.mock(BluetoothLeScanner::class.java)
    val peer = Mockito.mock(BluetoothDevice::class.java)
    val result = Mockito.mock(ScanResult::class.java)
    val callbacks = mutableListOf<ScanCallback>()
    val entered = CountDownLatch(1)
    val release = CountDownLatch(1)
    val workerFailure = AtomicReference<Throwable?>()
    Mockito.`when`(harness.adapter.bluetoothLeScanner).thenReturn(scanner)
    Mockito.`when`(result.device).thenReturn(peer)
    Mockito.`when`(peer.address).thenAnswer {
      entered.countDown()
      check(release.await(5, TimeUnit.SECONDS))
      "AA:BB:CC:DD:EE:01"
    }
    Mockito.doAnswer { callbacks.add(it.getArgument(2)); null }
      .`when`(scanner).startScan(Mockito.isNull(), Mockito.any(ScanSettings::class.java), Mockito.any(ScanCallback::class.java))
    mockJsi(harness).use {
      Mockito.mockConstruction(ScanSettings.Builder::class.java) { builder, _ ->
        Mockito.`when`(builder.setScanMode(Mockito.anyInt())).thenReturn(builder)
        Mockito.`when`(builder.setCallbackType(Mockito.anyInt())).thenReturn(builder)
        Mockito.`when`(builder.setReportDelay(Mockito.anyLong())).thenReturn(builder)
        Mockito.`when`(builder.build()).thenReturn(Mockito.mock(ScanSettings::class.java))
      }.use {
        val dispatcher = harness.dispatcher(harness.healthyFactory())
        try {
          dispatcher.dispatch(scanStartCommand(1, "a"))
          assertEquals(1, callbacks.size)
          val worker = Thread {
            try {
              Mockito.mockStatic(UnifiedBleProtocolJsiBinding::class.java).use { workerJsi ->
                if (failure) {
                  Mockito.mockStatic(Log::class.java).use { log ->
                    log.`when`<Int> { Log.e(Mockito.anyString(), Mockito.anyString()) }.thenAnswer {
                      entered.countDown(); check(release.await(5, TimeUnit.SECONDS)); 0
                    }
                    callbacks[0].onScanFailed(7)
                  }
                } else callbacks[0].onScanResult(ScanSettings.CALLBACK_TYPE_ALL_MATCHES, result)
                workerJsi.verifyNoInteractions()
              }
            } catch (error: Throwable) { workerFailure.set(error) }
          }
          worker.start()
          try {
            assertTrue(entered.await(5, TimeUnit.SECONDS))
            dispatcher.dispatch(stopScanCommand(2, "stop-a"))
            dispatcher.dispatch(scanStartCommand(3, "b"))
            assertEquals(2, callbacks.size)
            release.countDown()
            worker.join(5000)
            assertTrue(!worker.isAlive)
            workerFailure.get()?.let { throw AssertionError("callback failed", it) }
            Mockito.verify(scanner, Mockito.never()).stopScan(callbacks[1])
            callbacks[0].onScanFailed(8)
            Mockito.verify(scanner, Mockito.never()).stopScan(callbacks[1])
          } finally { release.countDown(); worker.join(5000) }
        } finally { dispatcher.close() }
      }
    }
  }

  private val direct = Executor { command -> command.run() }

  private class FakeJni(
    var linkedRevision: String = UbmGattCoreBinding.CONTRACT_REVISION,
    drains: List<String> = emptyList()
  ) : UbmGattCoreBinding.CoreJni {
    val enqueued = Collections.synchronizedList(mutableListOf<String>())
    var onScanStart: (() -> Unit)? = null
    private val drainQueue = drains.toMutableList()
    private var lastAutomaticScanReply = -1

    override fun open(revision: String): Long = 7L
    override fun revision(): String = linkedRevision
    override fun enqueue(handle: Long, wire: String): Int {
      if (wire.startsWith("scan.start|")) onScanStart?.invoke()
      enqueued.add(wire)
      return enqueued.size
    }
    override fun drain(handle: Long): String {
      val event = enqueued.lastOrNull()?.substringBefore('|') ?: return ""
      val marker = "\"event\":\"$event\""
      val index = drainQueue.indexOfFirst { response -> response.contains(marker) }
      if (index < 0) {
        val ordinal = enqueued.size - 1
        if (ordinal == lastAutomaticScanReply) return ""
        if (event == "scan.start") {
          lastAutomaticScanReply = ordinal
          return "{\"ok\":true,\"event\":\"scan.start\",\"op\":\"fake-scan-$ordinal\",\"observations\":[{\"kind\":\"central.scan-start\",\"op\":\"fake-scan-$ordinal\",\"detail\":\"\"}],\"effects\":[]}"
        }
        if (event == "scan.platform-event" || event == "scan.platform-started" || event == "adapter.reset") {
          lastAutomaticScanReply = ordinal
          return "{\"ok\":true,\"event\":\"$event\",\"observations\":[],\"effects\":[]}"
        }
        return ""
      }
      val response = drainQueue.removeAt(index)
      if (event != "connect") return response
      val lease = enqueued.lastOrNull()?.split('|')?.getOrNull(2) ?: return response
      return response.replace(Regex("\\\"lease\\\":\\\"[^\\\"]+\\\""), "\\\"lease\\\":\\\"$lease\\\"")
    }
    override fun depth(handle: Long): Int = enqueued.size
    override fun close(handle: Long) {}

    fun drainsRemaining(): Int = drainQueue.size
  }

  private inner class Harness(
    val jni: FakeJni,
    val emitted: MutableList<ByteArray> = Collections.synchronizedList(mutableListOf()),
    val diagnostics: MutableList<String> = Collections.synchronizedList(mutableListOf()),
    val rejections: MutableList<GattObservation> = Collections.synchronizedList(mutableListOf())
  ) {
    val adapter: BluetoothAdapter = Mockito.mock(BluetoothAdapter::class.java)
    val device: BluetoothDevice = Mockito.mock(BluetoothDevice::class.java)
    val gatt: BluetoothGatt = Mockito.mock(BluetoothGatt::class.java)
    var callback: BluetoothGattCallback? = null
    val context: Context = permittedContext(adapter)

    init {
      Mockito.`when`(adapter.state).thenReturn(BluetoothAdapter.STATE_ON)
      Mockito.`when`(adapter.getRemoteDevice("AA:BB:CC:DD:EE:FF")).thenReturn(device)
      Mockito.`when`(device.address).thenReturn("AA:BB:CC:DD:EE:FF")
      Mockito.`when`(gatt.device).thenReturn(device)
      Mockito.doAnswer { invocation ->
        callback = invocation.getArgument(2)
        gatt
      }.`when`(device).connectGatt(eq(context), eq(false), any(), eq(BluetoothDevice.TRANSPORT_LE))
    }

    fun dispatcher(coreShadowFactory: ((Context, (GattObservation) -> Unit) -> UbmGattCoreBinding?)?): UnifiedBleProtocolAndroidDispatcher =
      UnifiedBleProtocolAndroidDispatcher(context, NATIVE_HANDLE, coreShadowFactory, null)

    fun healthyFactory(): (Context, (GattObservation) -> Unit) -> UbmGattCoreBinding? =
      { ctx, onRejection ->
        UbmGattCoreBinding(
          ctx,
          jni = jni,
          onCoreRejection = { observation ->
            rejections.add(observation)
            onRejection(observation)
          },
          worker = direct,
          clockMs = { 1000L }
        )
      }

    fun establish(dispatcher: UnifiedBleProtocolAndroidDispatcher) {
      dispatcher.dispatch(
        commandBytes(
          "connect", 10L, "establish",
          mapOf(
            10 to ProtocolWireValue.RecordValue(connectionRecord("AA:BB:CC:DD:EE:FF")),
            20 to ProtocolWireValue.StringValue("direct")
          )
        )
      )
      (callback ?: error("connectGatt callback was not captured")).onConnectionStateChange(
        gatt,
        BluetoothGatt.GATT_SUCCESS,
        BluetoothProfile.STATE_CONNECTED
      )
    }
  }

  // -- the three proofs ------------------------------------------------------

  @Test
  fun coreRejectionBindsRadioSuccessIsNeverReported() {
    val harness = Harness(
      FakeJni(
        drains = listOf(
          "{\"ok\":true,\"event\":\"connect\",\"peer\":\"public-address:AA:BB:CC:DD:EE:FF\",\"lease\":\"android-link-AA:BB:CC:DD:EE:FF\",\"generation\":\"test-generation\",\"op\":\"connection.connect\",\"effects\":[],\"observations\":[]}",
          "{\"ok\":false,\"event\":\"disconnect\",\"code\":\"link.busy\",\"domain\":\"connection\",\"operation\":\"gatt-drain\",\"detail\":\"teardown-refused\",\"effects\":[],\"observations\":[]}"
        )
      )
    )
    mockJsi(harness).use {
      val dispatcher = harness.dispatcher(harness.healthyFactory())
      try {
        harness.establish(dispatcher)
        dispatcher.dispatch(disconnectCommand(epoch = 11L, nonce = "rejection-binds"))
      } finally {
        dispatcher.close()
      }
    }
    assertTrue(
      "disconnect line must be enqueued: ${harness.jni.enqueued}",
      harness.jni.enqueued.any {
        it.startsWith("disconnect|public-address:AA:BB:CC:DD:EE:FF|android-link-AA:BB:CC:DD:EE:FF-")
      }
    )
    assertEquals(1, harness.rejections.size)
    assertEquals("link.busy", harness.rejections.single().code)
    val terminals = parseTerminals(harness.emitted.filter { parseRecord(it, 0).kindWire == RecordKind.RESULT.wireValue })
    assertEquals("connect plus rejected disconnect, was $terminals", 2, terminals.size)
    val rejected = terminals.filter { it.errorCode != null }
    assertEquals("exactly one failed disconnect, terminals=$terminals", 1, rejected.size)
    assertTrue(rejected.single().errorMessage.contains("link.busy"))
    assertTrue(terminals.none { it.resultKind == "accepted" && it.outcome == "succeeded" })
  }

  @Test
  fun unknownDisconnectIsRejectedBeforeCoreOrRadioTouch() {
    val harness = Harness(FakeJni())
    mockJsi(harness).use {
      val dispatcher = harness.dispatcher(harness.healthyFactory())
      try {
        dispatcher.dispatch(disconnectCommand(epoch = 14L, nonce = "unknown-disconnect"))
      } finally {
        dispatcher.close()
      }
    }
    assertTrue(
      "unknown disconnect must not enqueue a disconnect: ${harness.jni.enqueued}",
      harness.jni.enqueued.none { line -> line.startsWith("disconnect|") }
    )
    val terminals = parseTerminals(harness.emitted.filter { parseRecord(it, 0).kindWire == RecordKind.RESULT.wireValue })
    assertEquals(1, terminals.size)
    assertEquals(CoreCommandAuthority.CODE_REJECTED, terminals.single().errorCode)
  }

  @Test
  fun shadowUnavailableFailsLoudInsteadOfRadioOnlySuccess() {
    val harness = Harness(FakeJni())
    mockJsi(harness).use {
      // No binding can ever open: every demand retry finds nothing.
      val dispatcher = harness.dispatcher { _, _ -> null }
      try {
        dispatcher.dispatch(stopScanCommand(epoch = 12L, nonce = "shadow-unavailable"))
      } finally {
        dispatcher.close()
      }
    }
    // Note: static mocks are thread-local, so the gate's background opener
    // attempt (when it wins the race) diagnoses through the real native and
    // its duplicate is then suppressed by the gate's once-per-cause latch.
    // The deterministic contract is the terminal below, which quotes the
    // seam's recorded cause; diagnosis latch semantics themselves are pinned
    // single-threaded in DeferredCoreShadowCauseTest / DeferredCoreShadowTest.
    // Fail loud: one actionable terminal quoting the cause — never the
    // radio-only "accepted" success the mirror path would have emitted.
    val terminals = parseTerminals(harness.emitted.filter { parseRecord(it, 0).kindWire == RecordKind.RESULT.wireValue })
    assertEquals("one terminal, was $terminals", 1, terminals.size)
    assertEquals("failed", terminals.single().outcome)
    assertEquals(CoreCommandAuthority.CODE_UNAVAILABLE, terminals.single().errorCode)
    assertTrue(
      "terminal must quote the outage cause, was $terminals",
      terminals.single().errorMessage.contains("factory returned no binding")
    )
  }

  @Test
  fun healthyCoreAttestsTheCommandEndToEnd() {
    val harness = Harness(
      FakeJni(
        drains = listOf(
          "{\"ok\":true,\"event\":\"connect\",\"peer\":\"public-address:AA:BB:CC:DD:EE:FF\",\"lease\":\"android-link-AA:BB:CC:DD:EE:FF\",\"generation\":\"test-generation\",\"op\":\"connection.connect\",\"effects\":[],\"observations\":[]}",
          "{\"ok\":true,\"event\":\"disconnect\",\"effects\":[],\"observations\":[]}"
        )
      )
    )
    mockJsi(harness).use {
      val dispatcher = harness.dispatcher(harness.healthyFactory())
      try {
        harness.establish(dispatcher)
        dispatcher.dispatch(disconnectCommand(epoch = 13L, nonce = "attested"))
      } finally {
        dispatcher.close()
      }
    }
    assertTrue(
      "disconnect line must be enqueued: ${harness.jni.enqueued}",
      harness.jni.enqueued.any {
        it.startsWith("disconnect|public-address:AA:BB:CC:DD:EE:FF|android-link-AA:BB:CC:DD:EE:FF-")
      }
    )
    assertEquals("connect/disconnect drains consumed", 0, harness.jni.drainsRemaining())
    val terminals = parseTerminals(harness.emitted.filter { parseRecord(it, 0).kindWire == RecordKind.RESULT.wireValue })
    assertEquals("connect and disconnect terminals, was $terminals", 2, terminals.size)
    assertTrue("connect success missing: $terminals", terminals.any { it.resultKind == "connected" && it.outcome == "succeeded" })
    assertTrue("disconnect success missing: $terminals", terminals.any { it.resultKind == "accepted" && it.outcome == "succeeded" })
  }

  // -- harness ---------------------------------------------------------------

  private fun mockJsi(harness: Harness): org.mockito.MockedStatic<UnifiedBleProtocolJsiBinding> {
    // Static stubbing lives in Java (see JsiStaticStubs): Kotlin cannot
    // disambiguate MockedStatic.when overloads for SAM lambdas.
    val mocked = Mockito.mockStatic(UnifiedBleProtocolJsiBinding::class.java)
    JsiStaticStubs.stubEmitRecord(mocked, harness.emitted)
    JsiStaticStubs.stubEmitDiagnostic(mocked, harness.diagnostics)
    return mocked
  }

  private fun registryJsi(harness: Harness, registry: JsiStaticStubs.Registry): org.mockito.MockedStatic<UnifiedBleProtocolJsiBinding> {
    val mocked = Mockito.mockStatic(UnifiedBleProtocolJsiBinding::class.java)
    JsiStaticStubs.stubRegistry(mocked, registry, harness.emitted)
    JsiStaticStubs.stubEmitDiagnostic(mocked, harness.diagnostics)
    return mocked
  }

  private fun permittedContext(adapter: BluetoothAdapter): Context {
    val context = Mockito.mock(Context::class.java)
    Mockito.`when`(context.applicationContext).thenReturn(context)
    Mockito.`when`(context.checkSelfPermission(Mockito.anyString())).thenReturn(
      PackageManager.PERMISSION_GRANTED
    )
    val bluetoothManager = Mockito.mock(BluetoothManager::class.java)
    Mockito.`when`(context.getSystemService(Context.BLUETOOTH_SERVICE)).thenReturn(bluetoothManager)
    Mockito.`when`(bluetoothManager.adapter).thenReturn(adapter)
    return context
  }

  // -- command builders (canonical wire encoding) ----------------------------

  private fun scanOptionsRecord(): ProtocolWireRecord = ProtocolWireRecord(
    RecordKind.SCAN_OPTIONS,
    mapOf(
      1 to ProtocolWireValue.StringListValue(emptyList()),
      2 to ProtocolWireValue.BooleanValue(true),
      3 to ProtocolWireValue.SignedIntegerValue(1L),
      4 to ProtocolWireValue.SignedIntegerValue(0L),
      5 to ProtocolWireValue.BooleanValue(true)
    )
  )

  private fun disconnectCommand(epoch: Long, nonce: String): ByteArray = commandBytes(
    "disconnect",
    epoch,
    nonce,
    mapOf(10 to ProtocolWireValue.RecordValue(connectionRecord("AA:BB:CC:DD:EE:FF")))
  )

  private fun stopScanCommand(epoch: Long, nonce: String): ByteArray =
    commandBytes("scanStop", epoch, nonce, emptyMap())

  @Suppress("unused")
  private fun scanStartCommand(epoch: Long, nonce: String): ByteArray = commandBytes(
    "scanStart",
    epoch,
    nonce,
    mapOf(12 to ProtocolWireValue.RecordValue(scanOptionsRecord()))
  )

  companion object {
    private const val NATIVE_HANDLE = 0xA11CE1L
  }
}
