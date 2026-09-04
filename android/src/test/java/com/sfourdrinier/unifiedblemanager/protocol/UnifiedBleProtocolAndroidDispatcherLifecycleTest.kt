// android/src/test/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolAndroidDispatcherLifecycleTest.kt

package com.sfourdrinier.unifiedblemanager.protocol

import com.sfourdrinier.unifiedblemanager.protocol.generated.RecordKind
import com.sfourdrinier.unifiedblemanager.radio.nextUuidOccurrence
import com.sfourdrinier.unifiedblemanager.radio.resolveUuidOccurrence
import com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidGattRadio
import com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidGattRadio.GattSerialQueue
import com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidSubscriptionOwnership
import com.sfourdrinier.unifiedblemanager.radio.BondedPeerSnapshot
import com.sfourdrinier.unifiedblemanager.radio.normalizeBondedPeerSnapshots
import com.sfourdrinier.unifiedblemanager.radio.bondedPeerAdapterReadiness
import com.sfourdrinier.unifiedblemanager.radio.requiresImmediateGattTeardownOnAdapterState
import com.sfourdrinier.unifiedblemanager.radio.classifyAndroidGattOperationFailure
import com.sfourdrinier.unifiedblemanager.radio.classifyAndroidNotificationRegistrationFailure
import com.sfourdrinier.unifiedblemanager.radio.AndroidGattOperationFailure
import com.sfourdrinier.unifiedblemanager.radio.AndroidCccdSubmissionFailure
import com.sfourdrinier.unifiedblemanager.radio.AndroidNotificationRollbackRejected
import com.sfourdrinier.unifiedblemanager.radio.OwnedRadioTeardownFailure
import com.sfourdrinier.unifiedblemanager.radio.androidGattTerminalResult
import com.sfourdrinier.unifiedblemanager.radio.shouldAwaitAndroidCccdDisconnectEvidence
import org.junit.Assert.assertEquals
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.File
import java.nio.file.Files
import java.util.UUID
import java.util.ArrayDeque
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.CountDownLatch
import java.util.concurrent.atomic.AtomicInteger
import kotlin.concurrent.thread
import android.bluetooth.BluetoothDevice
import android.bluetooth.BluetoothAdapter
import android.bluetooth.le.ScanSettings

class UnifiedBleProtocolAndroidDispatcherLifecycleTest {
  @Test
  fun gattStatus19IsTypedAsLinkLossButOrdinaryCccdFailureIsNot() {
    val linkLoss = classifyAndroidGattOperationFailure("cccd-write", 19)
    assertTrue(linkLoss.isLinkLoss)
    assertEquals(19, linkLoss.gattStatus)

    val ordinaryFailure = classifyAndroidGattOperationFailure("cccd-write", 133)
    assertFalse(ordinaryFailure.isLinkLoss)
    assertEquals(133, ordinaryFailure.gattStatus)
  }

  @Test
  fun onlyProvisionalAsyncCccdFailuresAwaitAuthoritativeDisconnectEvidence() {
    assertTrue(
      shouldAwaitAndroidCccdDisconnectEvidence(
        classifyAndroidGattOperationFailure("cccd-write", 133)
      )
    )
    assertFalse(
      shouldAwaitAndroidCccdDisconnectEvidence(
        classifyAndroidGattOperationFailure("cccd-write", 19)
      )
    )
    assertFalse(
      shouldAwaitAndroidCccdDisconnectEvidence(
        classifyAndroidGattOperationFailure("descriptor-write", 133)
      )
    )
    assertFalse(
      shouldAwaitAndroidCccdDisconnectEvidence(
        IllegalStateException("ordinary failure")
      )
    )

    val radio = readAndroidSource(
      "android/src/main/java/com/sfourdrinier/unifiedblemanager/radio/OwnedAndroidGattRadio.kt"
    )
    val callback = radio.substring(
      radio.indexOf("override fun onDescriptorWrite"),
      radio.indexOf("@Deprecated(\"Deprecated in Java\")")
    )
    assertTrue(callback.contains("deferExactCccdFailure(descriptor, pending, failure)"))
  }

  @Test
  fun dispatcherOnlyMapsTypedLinkLossToConnectionLost() {
    val linkLoss = classifyAndroidGattOperationFailure("cccd-write", 19)
    assertEquals("connectionLost", androidGattOperationFailureCode(linkLoss, "subscriptionFailed"))
    assertEquals(19, androidGattOperationFailureStatus(linkLoss))

    val ordinaryFailure = classifyAndroidGattOperationFailure("cccd-write", 133)
    assertEquals("subscriptionFailed", androidGattOperationFailureCode(ordinaryFailure, "subscriptionFailed"))
    assertEquals(133, androidGattOperationFailureStatus(ordinaryFailure))

    // Matching an error message is intentionally insufficient: only the
    // typed callback failure may change the public terminal vocabulary.
    assertEquals(
      "subscriptionFailed",
      androidGattOperationFailureCode(IllegalStateException("onDescriptorWrite status=19"), "subscriptionFailed")
    )
    assertNull(androidGattOperationFailureStatus(IllegalStateException("onDescriptorWrite status=19")))
  }

  @Test
  fun synchronousNotificationRegistrationFailureIsTypedAsLinkLossDespiteStaleManagerState() {
    val failure = classifyAndroidNotificationRegistrationFailure(
      operation = "notification-registration"
    )
    assertEquals("connectionLost", androidGattOperationFailureCode(failure, "subscriptionFailed"))
    assertNull(androidGattOperationFailureStatus(failure))

    val ordinaryCccdFailure = classifyAndroidGattOperationFailure("cccd-write", 133)
    assertEquals(
      "subscriptionFailed",
      androidGattOperationFailureCode(ordinaryCccdFailure, "subscriptionFailed")
    )
    assertEquals(133, androidGattOperationFailureStatus(ordinaryCccdFailure))
  }

  @Test
  fun exactAndNonExactNotificationRegistrationFailuresShareTheTypedLinkLossClassifier() {
    val radio = readAndroidSource(
      "android/src/main/java/com/sfourdrinier/unifiedblemanager/radio/OwnedAndroidGattRadio.kt"
    )
    val nonExact = radio.substring(
      radio.indexOf("private fun setNotify("),
      radio.indexOf("/** Enables or disables an exact duplicate-safe")
    )
    val exact = radio.substring(
      radio.indexOf("private fun setNotifyTarget("),
      radio.indexOf("private fun rollbackNotifyRegistration")
    )

    assertTrue(nonExact.contains("classifyAndroidNotificationRegistrationFailure("))
    assertTrue(exact.contains("classifyAndroidNotificationRegistrationFailure("))
    assertFalse(nonExact.contains("isConnected("))
    assertFalse(exact.contains("isConnected("))
    assertFalse(radio.contains("fun isConnected("))
  }

  @Test
  fun disconnectStatus19PreservesTypedCccdFailureWhileDrainingBothSubscribePaths() {
    val radio = readAndroidSource(
      "android/src/main/java/com/sfourdrinier/unifiedblemanager/radio/OwnedAndroidGattRadio.kt"
    )
    val pendingDrain = radio.substring(
      radio.indexOf("private fun failPendingForDevice("),
      radio.indexOf("private fun findChar(")
    )
    val disconnectCallback = radio.substring(
      radio.indexOf("override fun onConnectionStateChange"),
      radio.indexOf("override fun onDescriptorWrite")
    )
    val nonExactSubscribe = radio.substring(
      radio.indexOf("private fun setNotify("),
      radio.indexOf("/** Enables or disables an exact duplicate-safe")
    )
    val exactSubscribe = radio.substring(
      radio.indexOf("private fun setNotifyTarget("),
      radio.indexOf("private fun rollbackNotifyRegistration")
    )
    val descriptorCallback = radio.substring(
      radio.indexOf("override fun onDescriptorWrite"),
      radio.indexOf("@Deprecated(\"Deprecated in Java\")")
    )

    assertTrue(pendingDrain.contains("gattStatus: Int?"))
    assertTrue(pendingDrain.contains("classifyAndroidGattOperationFailure(\"cccd-write\", gattStatus)"))
    assertTrue(pendingDrain.contains("if (key.startsWith(\"cccd:\")) failCccd else failUnit"))
    assertTrue(pendingDrain.contains("if (entry.value.subscriptionEnabled != null) failCccd else failUnit"))
    assertTrue(nonExactSubscribe.contains("pendingDesc[key]"))
    assertTrue(exactSubscribe.contains("exactCccdPending.putIfAbsent(cccd, pending)"))
    assertTrue(descriptorCallback.contains("classifyAndroidGattOperationFailure"))
    assertTrue(
      disconnectCallback.contains(
        "failPendingForDevice(key, \"disconnected status=\$status\", status)"
      )
    )
  }

  @Test
  fun cccdRollbackFailureDoesNotReplaceThePrimaryTypedGattFailure() {
    val radio = readAndroidSource(
      "android/src/main/java/com/sfourdrinier/unifiedblemanager/radio/OwnedAndroidGattRadio.kt"
    )
    val completion = radio.substring(
      radio.indexOf("private fun completeExactUnit("),
      radio.indexOf("private fun rollbackNotifyRegistration")
    )

    assertTrue(completion.contains("androidGattTerminalResult(result, rollbackFailure)"))
    assertTrue(completion.contains("registerRetryableCleanup(rollbackFailure.operation)"))
    assertTrue(completion.contains("reportCleanupFailure(rollbackFailure)"))
  }

  @Test
  fun cccdRollbackFailurePreservesTypedPrimaryAndKeepsSuccessfulPrimaryCleanupFailureBehavior() {
    val primary = classifyAndroidGattOperationFailure("cccd-write", 19)
    val rollbackFailure = OwnedRadioTeardownFailure(
      "cccdRollback:AA:BB",
      IllegalStateException("link is gone")
    )

    val typedFailure = androidGattTerminalResult(Result.failure<Unit>(primary), rollbackFailure)
    val typedError = typedFailure.exceptionOrNull()
    assertTrue(typedError === primary)
    assertTrue(typedError is AndroidGattOperationFailure)
    if (typedError !is AndroidGattOperationFailure) throw AssertionError("typed primary was flattened")
    assertTrue(typedError.isLinkLoss)

    val callbackFailure = classifyAndroidGattOperationFailure("cccd-write", 133)
    val callbackFailureWithGenericCleanupError = androidGattTerminalResult(
      Result.failure<Unit>(callbackFailure),
      rollbackFailure
    )
    assertTrue(callbackFailureWithGenericCleanupError.exceptionOrNull() === callbackFailure)

    val rejectedRegistrationRollback = OwnedRadioTeardownFailure(
      "cccdRollback:AA:BB",
      AndroidNotificationRollbackRejected()
    )
    val linkLossAfterCallbackFailure = androidGattTerminalResult(
      Result.failure<Unit>(callbackFailure),
      rejectedRegistrationRollback
    )
    val callbackError = linkLossAfterCallbackFailure.exceptionOrNull()
    assertTrue(callbackError is AndroidGattOperationFailure)
    if (callbackError !is AndroidGattOperationFailure) {
      throw AssertionError("CCCD callback failure plus rejected rollback was not classified")
    }
    assertTrue(callbackError.isLinkLoss)
    assertEquals(133, callbackError.gattStatus)

    val descriptorFailure = classifyAndroidGattOperationFailure("descriptor-write", 133)
    assertTrue(
      androidGattTerminalResult(
        Result.failure<Unit>(descriptorFailure),
        rejectedRegistrationRollback
      ).exceptionOrNull() === descriptorFailure
    )

    val successfulPrimary = androidGattTerminalResult(Result.success(Unit), rollbackFailure)
    assertTrue(successfulPrimary.isFailure)
    assertTrue(successfulPrimary.exceptionOrNull() is IllegalStateException)
    assertTrue(successfulPrimary.exceptionOrNull()?.message?.contains("CCCD rollback failed") == true)

    val ordinaryPrimary = IllegalStateException("ordinary CCCD failure")
    val ordinaryFailure = androidGattTerminalResult(Result.failure<Unit>(ordinaryPrimary), rollbackFailure)
    assertTrue(ordinaryFailure.exceptionOrNull() === ordinaryPrimary)

    val synchronousSubmission = AndroidCccdSubmissionFailure(133)
    assertEquals(133, synchronousSubmission.platformStatus)
    val linkLossAfterSubmission = androidGattTerminalResult(
      Result.failure<Unit>(synchronousSubmission),
      rejectedRegistrationRollback
    )
    val linkLossError = linkLossAfterSubmission.exceptionOrNull()
    assertTrue(linkLossError is AndroidGattOperationFailure)
    if (linkLossError !is AndroidGattOperationFailure) throw AssertionError("submission failure was not classified")
    assertTrue(linkLossError.isLinkLoss)
    assertTrue(
      androidGattTerminalResult(Result.failure(synchronousSubmission), null).exceptionOrNull() ===
        synchronousSubmission
    )
    val legacySubmission = AndroidCccdSubmissionFailure(null)
    assertNull(legacySubmission.platformStatus)
  }

  @Test
  fun exactCccdSubmissionRejectionIsMarkedBeforeRollbackClassification() {
    val radio = readAndroidSource(
      "android/src/main/java/com/sfourdrinier/unifiedblemanager/radio/OwnedAndroidGattRadio.kt"
    )
    val exactSubscribe = radio.substring(
      radio.indexOf("private fun setNotifyTarget("),
      radio.indexOf("private fun rollbackNotifyRegistration")
    )
    val completion = radio.substring(
      radio.indexOf("private fun completeExactUnit("),
      radio.indexOf("private fun rollbackNotifyRegistration")
    )

    assertTrue(exactSubscribe.contains("AndroidCccdSubmissionFailure"))
    assertTrue(exactSubscribe.contains("AndroidCccdSubmissionFailure(status)"))
    assertTrue(exactSubscribe.contains("AndroidCccdSubmissionFailure(null)"))
    assertTrue(completion.contains("androidGattTerminalResult(result, rollbackFailure)"))
  }

  @Test
  fun publicSubscribeUsesExactNotificationPathWhileLegacyPrivatePathStaysOrdinary() {
    val dispatcher = readAndroidSource(
      "android/src/main/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolAndroidDispatcher.kt"
    )
    val subscribe = dispatcher.substring(
      dispatcher.indexOf("private fun subscribe(command: ProtocolWireRecord, enable: Boolean)"),
      dispatcher.indexOf("private fun destroy(command: ProtocolWireRecord)")
    )
    assertTrue(subscribe.contains("radio.setNotifyExact("))
    assertFalse(subscribe.contains("radio.setNotify("))

    val radio = readAndroidSource(
      "android/src/main/java/com/sfourdrinier/unifiedblemanager/radio/OwnedAndroidGattRadio.kt"
    )
    val legacy = radio.substring(
      radio.indexOf("private fun setNotify("),
      radio.indexOf("/** Enables or disables an exact duplicate-safe")
    )
    assertTrue(legacy.contains("IllegalStateException(\"writeDescriptor failed to start status=\$status\")"))
    assertTrue(legacy.contains("IllegalStateException(\"writeDescriptor failed to start\")"))
    assertFalse(legacy.contains("AndroidCccdSubmissionFailure"))
  }

  @Test
  fun bondedPeerAdapterReadinessReturnsTypedFailuresInsteadOfAnEmptySuccess() {
    assertTrue(
      bondedPeerAdapterReadiness(
        adapterAvailable = false,
        connectPermissionGranted = true,
        adapterState = BluetoothDevice.ERROR
      ).isFailure
    )
    assertTrue(
      bondedPeerAdapterReadiness(
        adapterAvailable = true,
        connectPermissionGranted = false,
        adapterState = android.bluetooth.BluetoothAdapter.STATE_ON
      ).exceptionOrNull() is SecurityException
    )
    assertTrue(
      bondedPeerAdapterReadiness(
        adapterAvailable = true,
        connectPermissionGranted = true,
        adapterState = android.bluetooth.BluetoothAdapter.STATE_OFF
      ).isFailure
    )
    assertTrue(
      bondedPeerAdapterReadiness(
        adapterAvailable = true,
        connectPermissionGranted = true,
        adapterState = android.bluetooth.BluetoothAdapter.STATE_TURNING_ON
      ).isFailure
    )
    assertTrue(
      bondedPeerAdapterReadiness(
        adapterAvailable = true,
        connectPermissionGranted = true,
        adapterState = android.bluetooth.BluetoothAdapter.STATE_ON
      ).isSuccess
    )
  }

  @Test
  fun emptyBondedPeerResultCarriesItsCorrelationAndAnEmptyField23List() {
    val correlation = ProtocolWireRecord(
      RecordKind.OPERATION_CORRELATION,
      mapOf(
        1 to ProtocolWireValue.RecordValue(
          ProtocolWireRecord(RecordKind.ATTACHMENT, emptyMap())
        ),
        2 to ProtocolWireValue.UnsignedIntegerValue(41L),
        3 to ProtocolWireValue.StringValue("enumerate-empty")
      )
    )

    val result = bondedPeerResultRecord(correlation, emptyList())
    assertEquals(RecordKind.RESULT, result.kind)
    assertEquals(ProtocolWireValue.StringValue("bondedPeers"), result.fields[2])
    val terminal = result.fields[3]
    if (terminal !is ProtocolWireValue.RecordValue) {
      throw AssertionError("Bonded peer result is missing its terminal")
    }
    assertEquals(
      ProtocolWireValue.RecordValue(correlation),
      terminal.value.fields[1]
    )
    assertEquals(
      ProtocolWireValue.RecordListValue(emptyList()),
      result.fields[23]
    )
  }

  @Test
  fun concurrentBondedPeerSettlementsRemoveOnlyTheirExactPendingCommand() {
    val first = ProtocolWireRecord(RecordKind.COMMAND, emptyMap())
    val second = ProtocolWireRecord(RecordKind.COMMAND, emptyMap())
    val pending = ConcurrentHashMap(mapOf("first" to first, "second" to second))

    assertTrue(claimExactPendingCommand(pending, "first", first))
    assertEquals(second, pending["second"])
    val lateFirst = ProtocolWireRecord(RecordKind.COMMAND, emptyMap())
    assertTrue(!claimExactPendingCommand(pending, "second", lateFirst))
    assertEquals(second, pending["second"])
    assertTrue(!claimExactPendingCommand(pending, "first", first))
    assertTrue(claimExactPendingCommand(pending, "second", second))
    assertTrue(pending.isEmpty())
  }

  @Test
  fun concurrentBondedSuccessCancellationAndTeardownCanClaimOnlyOneTerminal() {
    val command = ProtocolWireRecord(RecordKind.COMMAND, emptyMap())
    val pending = ConcurrentHashMap(mapOf("bonded" to command))
    val ready = CountDownLatch(3)
    val start = CountDownLatch(1)
    val finished = CountDownLatch(3)
    val claims = AtomicInteger(0)

    repeat(3) {
      thread {
        ready.countDown()
        start.await()
        if (claimExactPendingCommand(pending, "bonded", command)) claims.incrementAndGet()
        finished.countDown()
      }
    }
    assertTrue(ready.await(5, java.util.concurrent.TimeUnit.SECONDS))
    start.countDown()
    assertTrue(finished.await(5, java.util.concurrent.TimeUnit.SECONDS))
    assertEquals(1, claims.get())
    assertTrue(pending.isEmpty())
  }

  @Test
  fun bondedPeerSnapshotsAreDeterministicAndDeduplicated() {
    assertEquals(
      listOf(
        BondedPeerSnapshot("AA:BB", "Alpha"),
        BondedPeerSnapshot("CC:DD", null)
      ),
      normalizeBondedPeerSnapshots(
        listOf(
          BondedPeerSnapshot(" cc:dd ", ""),
          BondedPeerSnapshot("aa:bb", "Zulu"),
          BondedPeerSnapshot("AA:BB", "Alpha"),
          BondedPeerSnapshot("CC:DD", null)
        )
      )
    )
    val records = bondedPeerSnapshotRecords(
      listOf(BondedPeerSnapshot("AA:BB", "Alpha"), BondedPeerSnapshot("CC:DD", null))
    )
    assertEquals(2, records.size)
    assertEquals(ProtocolWireValue.StringValue("AA:BB"), records.first().fields[1])
    assertEquals(ProtocolWireValue.StringValue("Alpha"), records.first().fields[2])
    assertEquals(ProtocolWireValue.StringValue("CC:DD"), records.last().fields[1])
    assertTrue(!records.last().fields.containsKey(2))
  }

  @Test
  fun bondedPeerSnapshotOfAccessibleAdapterCanBeEmpty() {
    assertEquals(emptyList<BondedPeerSnapshot>(), normalizeBondedPeerSnapshots(emptyList()))
  }

  @Test
  fun bondedPeerEnumerationDoesNotAcquireConnectionOrScanOwnership() {
    val dispatcher = readAndroidSource(
      "android/src/main/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolAndroidDispatcher.kt"
    )
    val enumeration = dispatcher.substring(
      dispatcher.indexOf("private fun enumerateBondedPeers"),
      dispatcher.indexOf("private fun emitSuccess")
    )

    assertTrue(enumeration.contains("bondedPeerSnapshots"))
    assertTrue(enumeration.contains("RecordListValue"))
    assertFalse(enumeration.contains("pendingConnects"))
    assertFalse(enumeration.contains("establishedConnections"))
    assertFalse(enumeration.contains("activeScanCommand"))
    assertFalse(enumeration.contains("connectionPath"))
    assertFalse(enumeration.contains("lease"))
    assertFalse(enumeration.contains("registerBondStateReceiver"))
    assertFalse(enumeration.contains("registerAdapterStateReceiver"))
    assertFalse(enumeration.contains("connectGatt"))
  }

  @Test
  fun bondedPeerEnumerationFailsClosedForPermissionAndUnavailableAdapterStates() {
    val radio = readAndroidSource(
      "android/src/main/java/com/sfourdrinier/unifiedblemanager/radio/OwnedAndroidGattRadio.kt"
    )
    val enumeration = radio.substring(
      radio.indexOf("internal fun bondedPeerSnapshots"),
      radio.indexOf("/**\n   * Register [BluetoothAdapter.ACTION_STATE_CHANGED]")
    )

    assertTrue(enumeration.contains("hasBluetoothConnectPermission()"))
    assertFalse(enumeration.contains("emptyList"))
    assertTrue(radio.contains("Result.failure(SecurityException"))
    assertTrue(radio.contains("BluetoothAdapter.STATE_OFF"))
    assertTrue(radio.contains("BluetoothAdapter.STATE_TURNING_ON"))
    assertTrue(radio.contains("Bluetooth adapter is resetting"))
  }

  @Test
  fun bondedPeerTerminalUsesTheCommandCorrelationAndExactPendingRemoval() {
    val dispatcher = readAndroidSource(
      "android/src/main/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolAndroidDispatcher.kt"
    )
    val enumeration = dispatcher.substring(
      dispatcher.indexOf("private fun enumerateBondedPeers"),
      dispatcher.indexOf("private fun emitSuccess")
    )
    val success = dispatcher.substring(
      dispatcher.indexOf("private fun emitSuccess"),
      dispatcher.indexOf("private fun emitFailure")
    )

    assertTrue(enumeration.contains("emitSuccess(\n      command,\n      \"bondedPeers\""))
    assertTrue(success.contains("if (!isPending(command)) return"))
    assertTrue(success.contains("claimExactPendingCommand(pendingCommands, operationKey(command), command)"))
    assertTrue(dispatcher.contains("\"enumerateBondedPeers\" -> \"bondedPeers\""))
    val cancellation = dispatcher.substring(
      dispatcher.indexOf("private fun emitCancelled"),
      dispatcher.indexOf("private fun emitCancellationAcknowledgement")
    )
    assertTrue(cancellation.contains("claimExactPendingCommand(pendingCommands, operationKey(command), command)"))
    val failure = dispatcher.substring(
      dispatcher.indexOf("private fun emitFailure"),
      dispatcher.indexOf("private fun emitCancelled")
    )
    assertTrue(
      failure.indexOf("claimExactPendingCommand") <
        failure.indexOf("UnifiedBleProtocolJsiBinding.emitRecord")
    )
    assertTrue(dispatcher.contains("pendingBeforeDestroy = pendingCommands.values"))
    assertTrue(
      success.indexOf("claimExactPendingCommand") <
        success.indexOf("UnifiedBleProtocolJsiBinding.emitRecord")
    )
  }

  @Test
  fun cccdFailureClaimsThePendingCommandBeforeEmittingItsTerminal() {
    val dispatcher = readAndroidSource(
      "android/src/main/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolAndroidDispatcher.kt"
    )
    val failure = dispatcher.substring(
      dispatcher.indexOf("private fun emitFailure"),
      dispatcher.indexOf("private fun emitCancelled")
    )

    // Link loss and the CCCD callback can run concurrently. Claiming before
    // emission makes the first terminal the owner; a late subscriptionFailed
    // callback must not pass a check-then-emit race and publish a duplicate.
    val claimIndex = failure.indexOf(
      "if (!claimExactPendingCommand(pendingCommands, operationKey(command), command)) return",
      failure.indexOf("Claim before constructing")
    )
    val emitIndex = failure.indexOf("UnifiedBleProtocolJsiBinding.emitRecord")
    assertTrue(claimIndex >= 0)
    assertTrue(claimIndex < emitIndex)
  }

  @Test
  fun bondedPeerSnapshotBridgeHasAnOwnedRadioAndDispatcherRoute() {
    val radio = readAndroidSource(
      "android/src/main/java/com/sfourdrinier/unifiedblemanager/radio/OwnedAndroidGattRadio.kt"
    )
    val dispatcher = readAndroidSource(
      "android/src/main/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolAndroidDispatcher.kt"
    )

    assertTrue(radio.contains("data class BondedPeerSnapshot"))
    assertTrue(radio.contains("getBondedDevices()"))
    assertTrue(radio.contains("Manifest.permission.BLUETOOTH_CONNECT"))
    assertTrue(dispatcher.contains("enumerateBondedPeers"))
    assertEquals("bondedPeers", dispatcherResultKindFor("enumerateBondedPeers"))
  }

  @Test
  fun hostedAndroidCompileSeamsStayTypedUnambiguousAndAutoConnectPreserving() {
    val dispatcher = readAndroidSource(
      "android/src/main/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolAndroidDispatcher.kt"
    )
    val decoder = readAndroidSource(
      "android/src/main/java/com/sfourdrinier/unifiedblemanager/protocol/ProtocolCommandDecoder.kt"
    )
    val radio = readAndroidSource(
      "android/src/main/java/com/sfourdrinier/unifiedblemanager/radio/OwnedAndroidGattRadio.kt"
    )

    assertEquals(
      0,
      Regex("private fun ProtocolWireRecord\\.optionalString\\(fieldId: Int\\): String\\?")
        .findAll(dispatcher)
        .count()
    )
    assertEquals(
      1,
      Regex("internal fun ProtocolWireRecord\\.optionalString\\(fieldId: Int\\): String\\?")
        .findAll(decoder)
        .count()
    )
    assertTrue(dispatcher.contains("command.optionalString(17)"))
    assertTrue(dispatcher.contains("command.optionalString(18)"))
    assertTrue(dispatcher.contains("Build.VERSION.SDK_INT >= Build.VERSION_CODES.O"))
    assertTrue(Regex("internal fun readPhy\\(deviceId: String, onResult: \\(Result<OwnedAndroidPhy>").containsMatchIn(radio))
    assertTrue(Regex("internal fun requestPhy\\(").containsMatchIn(radio))
    assertTrue(radio.contains("Result<OwnedAndroidPhy?>"))
    assertTrue(radio.contains("gatt.readPhy()"))
    assertTrue(radio.contains("catch (error: Throwable)"))
    assertTrue(radio.contains("connectGatt(context, autoConnect"))
    assertTrue(radio.contains("ScanSettings.PHY_LE_ALL_SUPPORTED"))
    assertTrue(!radio.contains("BluetoothDevice.PHY_LE_ALL_SUPPORTED"))
    assertTrue(!radio.contains("!not"))
  }

  @Test
  fun dispatcherForwardsOwnedRadioServiceChangesAsDatabaseChangedEvents() {
    val dispatcher = readAndroidSource(
      "android/src/main/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolAndroidDispatcher.kt"
    )

    assertTrue(dispatcher.contains("radio.onServicesChanged = { deviceId ->"))
    assertTrue(dispatcher.contains("activeDatabases"))
    assertTrue(dispatcher.contains("databaseChangedEvent"))
  }

  @Test
  fun connectDecodesAbi6IntentWithoutAnImplicitStringDefault() {
    val dispatcher = readAndroidSource(
      "android/src/main/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolAndroidDispatcher.kt"
    )

    assertTrue(dispatcher.contains("command.requiredString(20)"))
    assertTrue(dispatcher.contains("ConnectionIntents.DIRECT"))
    assertTrue(dispatcher.contains("ConnectionIntents.WHEN_AVAILABLE"))
    assertTrue(dispatcher.contains("radio.connect(peerId, autoConnect)"))
    assertFalse(dispatcher.contains("command.optionalString(20) ?: \"direct\""))
  }

  @Test
  fun serviceChangedInvalidatesNativeSubscriptionOwnershipAndRoutesBeforeDatabaseChanged() {
    val dispatcher = readAndroidSource(
      "android/src/main/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolAndroidDispatcher.kt"
    )
    val radio = readAndroidSource(
      "android/src/main/java/com/sfourdrinier/unifiedblemanager/radio/OwnedAndroidGattRadio.kt"
    )

    assertTrue(dispatcher.contains("activeSubscriptions.remove"))
    assertTrue(dispatcher.contains("clearSubscriptionRoutesForDevice"))
    assertTrue(radio.contains("activeNativeSubscriptionOwnership"))
    assertTrue(radio.contains("invalidateForDatabaseChange"))
    assertTrue(
      dispatcher.indexOf("clearSubscriptionRoutesForDevice(deviceId)") <
        dispatcher.indexOf("emitDatabaseChanged(database)")
    )
    assertTrue(
      radio.indexOf("invalidateNativeSubscriptionsForDatabaseChange(key, gatt, generation)") <
        radio.indexOf("onServicesChanged?.invoke(id)")
    )
  }

  @Test
  fun nativeSubscriptionOwnershipIsBoundToTheGattGenerationAndInvalidatedForThatGeneration() {
    val ownership = OwnedAndroidSubscriptionOwnership<String>()
    ownership.activate("AA:BB", 7L, "heart-rate")
    ownership.activate("CC:DD", 3L, "battery")

    assertTrue(ownership.isActive("AA:BB", 7L, "heart-rate"))
    assertTrue(!ownership.isActive("AA:BB", 8L, "heart-rate"))
    assertEquals(listOf("heart-rate"), ownership.invalidateForDatabaseChange("AA:BB", 7L))
    assertTrue(!ownership.isActive("AA:BB", 7L, "heart-rate"))
    assertTrue(ownership.isActive("CC:DD", 3L, "battery"))
  }

  @Test
  fun databaseChangedEventRetainsTheCurrentAttachmentAndDatabasePath() {
    val attachment = ProtocolWireRecord(
      RecordKind.ATTACHMENT,
      mapOf(
        1 to ProtocolWireValue.StringValue("attachment-1"),
        2 to ProtocolWireValue.StringValue("backend-1"),
        3 to ProtocolWireValue.StringValue("generation-1"),
        4 to ProtocolWireValue.StringValue("adapter-1"),
        5 to ProtocolWireValue.StringValue("adapter-generation-1")
      )
    )
    val connection = ProtocolWireRecord(
      RecordKind.CONNECTION_PATH,
      mapOf(
        1 to ProtocolWireValue.RecordValue(attachment),
        2 to ProtocolWireValue.StringValue("C0FFEE000001"),
        3 to ProtocolWireValue.StringValue("connection-1"),
        4 to ProtocolWireValue.StringValue("lease-1"),
        5 to ProtocolWireValue.StringValue("connection-generation-1")
      )
    )
    val database = ProtocolWireRecord(
      RecordKind.DATABASE_PATH,
      mapOf(
        1 to ProtocolWireValue.RecordValue(connection),
        2 to ProtocolWireValue.StringValue("database-1"),
        3 to ProtocolWireValue.StringValue("database-generation-1")
      )
    )

    val event = databaseChangedEvent(17L, database, 3L, 99L)

    assertEquals(RecordKind.EVENT, event.kind)
    assertEquals(ProtocolWireValue.StringValue("databaseChanged"), event.fields[3])
    assertEquals(ProtocolWireValue.RecordValue(attachment), event.fields[4])
    assertEquals(ProtocolWireValue.UnsignedIntegerValue(3L), event.fields[5])
    assertEquals(ProtocolWireValue.UnsignedIntegerValue(99L), event.fields[6])
    assertEquals(ProtocolWireValue.RecordValue(database), event.fields[8])
  }

  @Test
  fun androidPhyWireValuesMapToFailClosedPlatformMasks() {
    assertEquals(BluetoothDevice.PHY_LE_1M, OwnedAndroidGattRadio.phyValue("le1m"))
    assertEquals(BluetoothDevice.PHY_LE_2M, OwnedAndroidGattRadio.phyValue("le2m"))
    assertEquals(BluetoothDevice.PHY_LE_CODED, OwnedAndroidGattRadio.phyValue("leCoded"))
    assertEquals(ScanSettings.PHY_LE_ALL_SUPPORTED, OwnedAndroidGattRadio.phyValue(null))
    assertEquals(0, OwnedAndroidGattRadio.phyMaskValue(null))
    assertEquals(BluetoothDevice.PHY_LE_1M_MASK, OwnedAndroidGattRadio.phyMaskValue("le1m"))
    assertEquals(BluetoothDevice.PHY_LE_2M_MASK, OwnedAndroidGattRadio.phyMaskValue("le2m"))
    assertEquals(BluetoothDevice.PHY_LE_CODED_MASK, OwnedAndroidGattRadio.phyMaskValue("leCoded"))
    var rejected = false
    try {
      OwnedAndroidGattRadio.phyValue("unknown")
    } catch (_: IllegalArgumentException) {
      rejected = true
    }
    assertTrue(rejected)
  }

  @Test
  fun securityPairTransportIsRequiredAndLeUsesTheExplicitAndroidTransport() {
    val dispatcher = readAndroidSource(
      "android/src/main/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolAndroidDispatcher.kt"
    )
    val radio = readAndroidSource(
      "android/src/main/java/com/sfourdrinier/unifiedblemanager/radio/OwnedAndroidGattRadio.kt"
    )

    assertTrue(dispatcher.contains("val pairTransport = command.requiredString(19)"))
    assertTrue(dispatcher.contains("radio.pair(peerId, pairTransport)"))
    assertTrue(radio.contains("isAlreadyPaired(device.bondState, device.type, transport)"))
    assertTrue(radio.contains("\"platformDefault\" -> device.createBond()"))
    assertTrue(radio.contains("method.invoke(device, BluetoothDevice.TRANSPORT_LE)"))
    assertTrue(radio.contains("catch (error: InvocationTargetException)"))
    assertTrue(radio.contains("is SecurityException -> throw cause"))
    assertTrue(radio.contains("else -> throw IllegalArgumentException"))
  }

  @Test
  fun explicitLeAlreadyPairedRequiresAnUnambiguouslyLeOnlyBond() {
    assertTrue(OwnedAndroidGattRadio.isAlreadyPaired(
      BluetoothDevice.BOND_BONDED,
      BluetoothDevice.DEVICE_TYPE_CLASSIC,
      "platformDefault"
    ))
    assertTrue(OwnedAndroidGattRadio.isAlreadyPaired(
      BluetoothDevice.BOND_BONDED,
      BluetoothDevice.DEVICE_TYPE_LE,
      "le"
    ))
    assertFalse(OwnedAndroidGattRadio.isAlreadyPaired(
      BluetoothDevice.BOND_BONDED,
      BluetoothDevice.DEVICE_TYPE_DUAL,
      "le"
    ))
    assertFalse(OwnedAndroidGattRadio.isAlreadyPaired(
      BluetoothDevice.BOND_BONDED,
      BluetoothDevice.DEVICE_TYPE_CLASSIC,
      "le"
    ))
    assertFalse(OwnedAndroidGattRadio.isAlreadyPaired(
      BluetoothDevice.BOND_NONE,
      BluetoothDevice.DEVICE_TYPE_LE,
      "le"
    ))
  }

  @Test
  fun sourceGuardReadsAndroidSourcesFromInstalledConsumerPackageLayout() {
    val relativePath = "android/src/main/java/com/sfourdrinier/unifiedblemanager/protocol/PackagedSourceGuardTarget.kt"
    val consumerRoot = Files.createTempDirectory("unified-ble-source-guard-").toFile()
    val consumerAndroidDirectory = File(consumerRoot, "android")
    val packagedSource = File(consumerRoot, "node_modules/unified-ble-manager/$relativePath")
    consumerAndroidDirectory.mkdirs()
    requireNotNull(packagedSource.parentFile).mkdirs()
    packagedSource.writeText("installed-package-source")

    try {
      assertEquals(
        "installed-package-source",
        readAndroidSource(relativePath, consumerAndroidDirectory)
      )
    } finally {
      consumerRoot.deleteRecursively()
    }
  }

  private fun readAndroidSource(relativePath: String, workingDirectory: File = File(".")): String {
    val candidates = listOf(
      File(workingDirectory, relativePath),
      File(workingDirectory, "../$relativePath"),
      File(workingDirectory, "../../$relativePath"),
      File(workingDirectory, "../../../$relativePath"),
      File(workingDirectory, "../node_modules/unified-ble-manager/$relativePath")
    )
    return candidates.firstOrNull { it.isFile }?.readText()
      ?: throw AssertionError("Unable to locate Android source guard target: $relativePath")
  }

  @Test
  fun duplicateInstallReservationDoesNotConstructALoserAndCloseReturnsOwnerCountsToZero() {
    class TrackingOwner(private val release: () -> Unit) {
      fun close() = release()
    }

    val registry = UnifiedBleProtocolJsiBinding.DispatcherInstallRegistry<TrackingOwner>()
    var dispatcherOwners = 0
    var receiverOwners = 0
    var constructionCount = 0
    val owner = registry.reserve(91L) {
      constructionCount += 1
      dispatcherOwners += 1
      receiverOwners += 1
      TrackingOwner {
        dispatcherOwners -= 1
        receiverOwners -= 1
      }
    }

    var duplicateRejected = false
    try {
      registry.reserve(91L) {
        constructionCount += 1
        dispatcherOwners += 1
        receiverOwners += 1
        TrackingOwner {
          dispatcherOwners -= 1
          receiverOwners -= 1
        }
      }
    } catch (_: IllegalStateException) {
      duplicateRejected = true
    }

    assertTrue(duplicateRejected)
    assertEquals(1, constructionCount)
    assertEquals(1, registry.ownerCount())
    assertEquals(1, dispatcherOwners)
    assertEquals(1, receiverOwners)
    assertTrue(registry.removeExact(91L, owner))
    owner.close()
    assertEquals(0, registry.ownerCount())
    assertEquals(0, dispatcherOwners)
    assertEquals(0, receiverOwners)
  }

  @Test
  fun failedAttachmentCloseRetainsTheExactDispatcherUntilItsRadioTeardownRetriesSuccessfully() {
    class TrackingOwner {
      var closeAttempts = 0
    }

    val registry = UnifiedBleProtocolJsiBinding.DispatcherInstallRegistry<TrackingOwner>()
    val owner = registry.reserve(92L) { TrackingOwner() }

    assertTrue(
      !registry.closeRetainingOwner(92L) { retained ->
        retained.closeAttempts += 1
        false
      }
    )
    assertEquals(owner, registry.get(92L))
    assertEquals(1, owner.closeAttempts)
    assertEquals(1, registry.ownerCount())

    var duplicateRejected = false
    try {
      registry.reserve(92L) { TrackingOwner() }
    } catch (_: IllegalStateException) {
      duplicateRejected = true
    }
    assertTrue(duplicateRejected)

    assertTrue(
      registry.closeRetainingOwner(92L) { retained ->
        retained.closeAttempts += 1
        true
      }
    )
    assertNull(registry.get(92L))
    assertEquals(2, owner.closeAttempts)
    assertEquals(0, registry.ownerCount())
  }

  @Test
  fun decodesTheCanonicalDestroyCommandUsedByTheDispatcher() {
    val attachment = ProtocolWireRecord(
      RecordKind.ATTACHMENT,
      mapOf(
        1 to ProtocolWireValue.StringValue("attachment-1"),
        2 to ProtocolWireValue.StringValue("android-native-protocol-test"),
        3 to ProtocolWireValue.StringValue("generation-1"),
        4 to ProtocolWireValue.StringValue("adapter-1"),
        5 to ProtocolWireValue.StringValue("adapter-generation-1")
      )
    )
    val correlation = ProtocolWireRecord(
      RecordKind.OPERATION_CORRELATION,
      mapOf(
        1 to ProtocolWireValue.RecordValue(attachment),
        2 to ProtocolWireValue.UnsignedIntegerValue(7L),
        3 to ProtocolWireValue.StringValue("destroy-command")
      )
    )
    val command = ProtocolWireRecord(
      RecordKind.COMMAND,
      mapOf(
        1 to ProtocolWireValue.UnsignedIntegerValue(1L),
        2 to ProtocolWireValue.RecordValue(correlation),
        3 to ProtocolWireValue.StringValue("destroy")
      )
    )

    assertEquals(command, ProtocolCommandDecoder.decodeCommand(ProtocolWireEncoder.encode(command)))
  }

  @Test
  fun mapsDispatcherCommandsToTheirCanonicalResultKinds() {
    assertEquals("scanStarted", dispatcherResultKindFor("scanStart"))
    assertEquals("connected", dispatcherResultKindFor("connect"))
    assertEquals("database", dispatcherResultKindFor("discover"))
    assertEquals("read", dispatcherResultKindFor("read"))
    assertEquals("descriptorWrite", dispatcherResultKindFor("writeDescriptor"))
    assertEquals("destroyed", dispatcherResultKindFor("destroy"))
    assertEquals("accepted", dispatcherResultKindFor("disconnect"))
  }

  @Test
  fun connectionLostEventPreservesCanonicalConnectionAndTerminalStatus() {
    val attachment = ProtocolWireRecord(
      RecordKind.ATTACHMENT,
      mapOf(
        1 to ProtocolWireValue.StringValue("attachment-1"),
        2 to ProtocolWireValue.StringValue("backend-1"),
        3 to ProtocolWireValue.StringValue("generation-1"),
        4 to ProtocolWireValue.StringValue("adapter-1"),
        5 to ProtocolWireValue.StringValue("adapter-generation-1")
      )
    )
    val connection = ProtocolWireRecord(
      RecordKind.CONNECTION_PATH,
      mapOf(
        1 to ProtocolWireValue.RecordValue(attachment),
        2 to ProtocolWireValue.StringValue("C0FFEE000001"),
        3 to ProtocolWireValue.StringValue("connection-1"),
        4 to ProtocolWireValue.StringValue("lease-1"),
        5 to ProtocolWireValue.StringValue("connection-generation-1")
      )
    )

    val event = connectionLostEvent(17L, connection, 133, 3L, 99L)

    assertEquals(RecordKind.EVENT, event.kind)
    assertEquals(ProtocolWireValue.StringValue("connectionLost"), event.fields[3])
    assertEquals(ProtocolWireValue.RecordValue(attachment), event.fields[4])
    assertEquals(ProtocolWireValue.RecordValue(connection), event.fields[7])
    assertEquals(ProtocolWireValue.UnsignedIntegerValue(3L), event.fields[5])
    assertEquals(ProtocolWireValue.UnsignedIntegerValue(99L), event.fields[6])
    val errorValue = event.fields[14]
    if (errorValue !is ProtocolWireValue.RecordValue) {
      throw AssertionError("Connection-lost event is missing its canonical error record")
    }
    val error = errorValue.value
    assertEquals(ProtocolWireValue.StringValue("connectionLost"), error.fields[1])
    assertEquals(ProtocolWireValue.SignedIntegerValue(133L), error.fields[8])
    assertTrue(ProtocolWireEncoder.encode(event).isNotEmpty())
  }

  @Test
  fun duplicateOccurrencesAreScopedToEachUuidAtEveryGattContainmentLevel() {
    val serviceA = UUID.fromString("0000180d-0000-1000-8000-00805f9b34fb")
    val serviceB = UUID.fromString("0000180f-0000-1000-8000-00805f9b34fb")
    val characteristicA = UUID.fromString("00002a37-0000-1000-8000-00805f9b34fb")
    val characteristicB = UUID.fromString("00002a38-0000-1000-8000-00805f9b34fb")
    val descriptorA = UUID.fromString("00002902-0000-1000-8000-00805f9b34fb")
    val descriptorB = UUID.fromString("00002901-0000-1000-8000-00805f9b34fb")

    val serviceOrdinals = mutableMapOf<UUID, Int>()
    assertEquals(
      listOf(0, 0, 1),
      listOf(serviceA, serviceB, serviceA).map { uuid -> nextUuidOccurrence(serviceOrdinals, uuid) }
    )
    val characteristicOrdinals = mutableMapOf<UUID, Int>()
    assertEquals(
      listOf(0, 0, 1),
      listOf(characteristicA, characteristicB, characteristicA)
        .map { uuid -> nextUuidOccurrence(characteristicOrdinals, uuid) }
    )
    val descriptorOrdinals = mutableMapOf<UUID, Int>()
    assertEquals(
      listOf(0, 0, 1),
      listOf(descriptorA, descriptorB, descriptorA)
        .map { uuid -> nextUuidOccurrence(descriptorOrdinals, uuid) }
    )
    assertEquals(
      serviceA,
      resolveUuidOccurrence(listOf(serviceA, serviceB, serviceA), serviceA, 1) { it }
    )
    assertEquals(
      characteristicA,
      resolveUuidOccurrence(listOf(characteristicA, characteristicB, characteristicA), characteristicA, 1) { it }
    )
    assertEquals(
      descriptorA,
      resolveUuidOccurrence(listOf(descriptorA, descriptorB, descriptorA), descriptorA, 1) { it }
    )
    assertNull(resolveUuidOccurrence(listOf(serviceA, serviceB, serviceA), serviceA, 2) { it })
  }

  @Test
  fun explicitSubscriptionModesNeverFallBackToAnotherCccdMode() {
    val notifyOnly = android.bluetooth.BluetoothGattCharacteristic.PROPERTY_NOTIFY
    val indicateOnly = android.bluetooth.BluetoothGattCharacteristic.PROPERTY_INDICATE

    assertArrayEquals(
      android.bluetooth.BluetoothGattDescriptor.ENABLE_NOTIFICATION_VALUE,
      com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidGattRadio.resolveCccdPayload(
        true,
        "notification",
        notifyOnly
      )
    )
    assertNull(
      com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidGattRadio.resolveCccdPayload(
        true,
        "notification",
        indicateOnly
      )
    )
    assertNull(
      com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidGattRadio.resolveCccdPayload(
        true,
        "unsupported-mode",
        notifyOnly or indicateOnly
      )
    )
  }

  @Test
  fun dispatcherForwardsResolvedSubscriptionModeToOwnedRadioCccdSelection() {
    val dispatcher = readAndroidSource(
      "android/src/main/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolAndroidDispatcher.kt"
    )
    val radio = readAndroidSource(
      "android/src/main/java/com/sfourdrinier/unifiedblemanager/radio/OwnedAndroidGattRadio.kt"
    )

    assertTrue(dispatcher.contains("subscriptionType = command.optionalString(21)"))
    assertTrue(dispatcher.contains("command.optionalString(21)\n            )"))
    assertTrue(dispatcher.contains("subscriptionType = route.mode"))
    assertTrue(radio.contains("resolveCccdPayload(enable, subscriptionType"))
  }

  @Test
  fun activeCancellationKeepsTheFifoBlockedUntilTheLateNativeCompletion() {
    val scheduled = ArrayDeque<() -> Unit>()
    val queue = GattSerialQueue(
      post = { task -> scheduled.addLast(task); true },
      idProvider = { 100L + scheduled.size.toLong() }
    )
    var firstStarted = 0
    var firstCancelled = 0
    var secondStarted = 0
    var firstDone: (() -> Unit)? = null
    val firstId = queue.submit({ done ->
      firstStarted += 1
      firstDone = done
    }) {
      firstCancelled += 1
    }
    queue.submit({
      secondStarted += 1
    }) {}

    scheduled.removeFirst().invoke()
    assertEquals(1, firstStarted)
    assertTrue(queue.cancel(firstId))
    assertEquals(1, firstCancelled)
    assertEquals(0, secondStarted)

    firstDone?.invoke()
    scheduled.removeFirst().invoke()
    assertEquals(1, secondStarted)
  }

  @Test
  fun gattSafetyClosePublishesConnectionLossBeforeDroppingNativeOwnership() {
    val radio = readAndroidSource(
      "android/src/main/java/com/sfourdrinier/unifiedblemanager/radio/OwnedAndroidGattRadio.kt"
    )
    val timeout = radio.substring(
      radio.indexOf("private fun scheduleSafeClose"),
      radio.indexOf("private fun cancelSafeClose")
    )

    val connectionLossIndex = timeout.indexOf("dispatchConnectionState(key, false, BluetoothGatt.GATT_FAILURE)")
    val failPendingIndex = timeout.indexOf("failPendingForDevice(key, \"disconnected timeout\")")
    val teardownIndex = timeout.indexOf("val teardownFailure = completeGattTeardown(key, gatt)")
    assertTrue(connectionLossIndex >= 0)
    assertTrue(connectionLossIndex < failPendingIndex)
    assertTrue(connectionLossIndex < teardownIndex)
  }

  @Test
  fun gattSafetyCloseTimeoutUsesInformationalDiagnostics() {
    val radio = readAndroidSource(
      "android/src/main/java/com/sfourdrinier/unifiedblemanager/radio/OwnedAndroidGattRadio.kt"
    )
    val timeout = radio.substring(
      radio.indexOf("private fun scheduleSafeClose"),
      radio.indexOf("private fun cancelSafeClose")
    )

    assertTrue(timeout.contains("OwnedAndroidLog.i(\"GATT close safety timeout"))
    assertFalse(timeout.contains("OwnedAndroidLog.w(\"GATT close safety timeout"))
    assertFalse(timeout.contains("OwnedAndroidLog.e(\"GATT close safety timeout"))
  }

  @Test
  fun adapterLossClosesGattOwnersBeforeForwardingTheState() {
    val radio = readAndroidSource(
      "android/src/main/java/com/sfourdrinier/unifiedblemanager/radio/OwnedAndroidGattRadio.kt"
    )
    val receiver = radio.substring(
      radio.indexOf("fun registerAdapterStateReceiver()"),
      radio.indexOf("internal fun unregisterAdapterStateReceiver")
    )

    val teardownIndex = receiver.indexOf("handleAdapterStateTransition(state)")
    val forwardIndex = receiver.indexOf("onAdapterState?.invoke(mapAdapterState(state))")
    assertTrue(teardownIndex >= 0)
    assertTrue(teardownIndex < forwardIndex)
  }

  @Test
  fun onlyUnavailableAdapterTransitionsRequireImmediateGattTeardown() {
    assertFalse(requiresImmediateGattTeardownOnAdapterState(BluetoothAdapter.STATE_ON))
    assertTrue(requiresImmediateGattTeardownOnAdapterState(BluetoothAdapter.STATE_OFF))
    assertTrue(requiresImmediateGattTeardownOnAdapterState(BluetoothAdapter.STATE_TURNING_OFF))
    assertTrue(requiresImmediateGattTeardownOnAdapterState(BluetoothAdapter.STATE_TURNING_ON))
  }

  @Test
  fun adapterLossTeardownDoesNotWaitForGattDisconnectedCallback() {
    val radio = readAndroidSource(
      "android/src/main/java/com/sfourdrinier/unifiedblemanager/radio/OwnedAndroidGattRadio.kt"
    )
    val transition = radio.substring(
      radio.indexOf("private fun handleAdapterStateTransition("),
      radio.indexOf("internal fun unregisterAdapterStateReceiver")
    )

    assertTrue(transition.contains("pendingReconnect.clear()"))
    assertTrue(transition.contains("failPendingForDevice(key, \"adapter unavailable\")"))
    assertTrue(transition.contains("completeGattTeardown(key, gatt)"))
    assertTrue(transition.contains("pendingDisconnectCallbacks.remove(key)?.invoke(teardownFailure)"))
    assertFalse(transition.contains("scheduleSafeClose"))
    assertFalse(transition.contains("dispatchConnectionState"))
  }

  @Test
  fun adapterRecoveryRetriesRetainedGattTeardownOwnership() {
    val radio = readAndroidSource(
      "android/src/main/java/com/sfourdrinier/unifiedblemanager/radio/OwnedAndroidGattRadio.kt"
    )
    val transition = radio.substring(
      radio.indexOf("private fun handleAdapterStateTransition("),
      radio.indexOf("internal fun unregisterAdapterStateReceiver")
    )

    assertTrue(transition.contains("BluetoothAdapter.STATE_ON"))
    assertTrue(transition.contains("pendingGattTeardowns.keys.toList()"))
    assertTrue(transition.contains("retryGattTeardown(key)"))
  }

  @Test
  fun disconnectRetriesRetainedOrUnavailableGattWithoutSchedulingSafetyClose() {
    val radio = readAndroidSource(
      "android/src/main/java/com/sfourdrinier/unifiedblemanager/radio/OwnedAndroidGattRadio.kt"
    )
    val disconnect = radio.substring(
      radio.indexOf("internal fun disconnect("),
      radio.indexOf("private fun openGatt")
    )

    val retainedIndex = disconnect.indexOf("pendingGattTeardowns[key]")
    val unavailableIndex = disconnect.indexOf("requiresImmediateGattTeardownOnAdapterState(adapter?.state)")
    val safetyCloseIndex = disconnect.indexOf("scheduleSafeClose(key, g)")
    assertTrue(retainedIndex >= 0)
    assertTrue(unavailableIndex > retainedIndex)
    assertTrue(safetyCloseIndex > unavailableIndex)
  }

  @Test
  fun adapterLossClearsProtocolGattRoutesBeforePublishingAdapterState() {
    val dispatcher = readAndroidSource(
      "android/src/main/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolAndroidDispatcher.kt"
    )
    val callback = dispatcher.substring(
      dispatcher.indexOf("radio.onAdapterState = {"),
      dispatcher.indexOf("radio.onCleanupFailure")
    )
    val clearIndex = callback.indexOf("clearGattProtocolOwnershipForAdapterLoss(adapterState)")
    val emitIndex = callback.indexOf("emitCurrentAdapterState()")
    assertTrue(clearIndex >= 0)
    assertTrue(clearIndex < emitIndex)

    val cleanup = dispatcher.substring(
      dispatcher.indexOf("private fun clearGattProtocolOwnershipForAdapterLoss(adapterState: String)"),
      dispatcher.indexOf("fun emitCurrentAdapterState()")
    )
    assertTrue(cleanup.contains("establishedConnections.clear()"))
    assertTrue(cleanup.contains("activeDatabases.clear()"))
    assertTrue(cleanup.contains("activeSubscriptions.clear()"))
    assertTrue(cleanup.contains("pendingConnects.entries.toList()"))
    assertTrue(cleanup.contains("pendingConnects.remove(entry.key, entry.value)"))
    assertTrue(cleanup.contains("emitFailure(entry.value, failure.code, failure.message)"))
  }

  @Test
  fun unsubscribeIsIdempotentAfterAdapterLossClearsNativeOwnership() {
    val dispatcher = readAndroidSource(
      "android/src/main/java/com/sfourdrinier/unifiedblemanager/protocol/UnifiedBleProtocolAndroidDispatcher.kt"
    )
    val subscribe = dispatcher.substring(
      dispatcher.indexOf("private fun subscribe(command: ProtocolWireRecord, enable: Boolean)"),
      dispatcher.indexOf("private fun destroy(command: ProtocolWireRecord)")
    )
    val absentRouteIndex = subscribe.indexOf("!enable && !activeSubscriptions.containsKey(subscriptionId)")
    val successIndex = subscribe.indexOf("emitSuccess(command, \"unsubscribed\")")
    val nativeIndex = subscribe.indexOf("radio.setNotifyExact(")
    assertTrue(absentRouteIndex >= 0)
    assertTrue(successIndex > absentRouteIndex)
    assertTrue(nativeIndex > successIndex)
  }

  @Test
  fun destroyBeforePumpCallsEveryQueuedCancellationAndNeverStartsThem() {
    val scheduled = ArrayDeque<() -> Unit>()
    val queue = GattSerialQueue(post = { task -> scheduled.addLast(task); true })
    var starts = 0
    var cancellations = 0
    queue.submit({ starts += 1 }) { cancellations += 1 }
    queue.submit({ starts += 1 }) { cancellations += 1 }

    queue.clear(IllegalStateException("destroy-before-pump"))
    assertEquals(0, starts)
    assertEquals(2, cancellations)
    scheduled.removeFirst().invoke()
    assertEquals(0, starts)
  }

  @Test
  fun radioScopedOperationTokensDoNotCollideAcrossDeviceQueues() {
    val scheduled = ArrayDeque<() -> Unit>()
    var nextToken = 1L
    val firstQueue = GattSerialQueue(
      post = { task -> scheduled.addLast(task); true },
      idProvider = { nextToken++ }
    )
    val secondQueue = GattSerialQueue(
      post = { task -> scheduled.addLast(task); true },
      idProvider = { nextToken++ }
    )
    var firstCancelled = 0
    var secondCancelled = 0
    firstQueue.submit({}) { firstCancelled += 1 }
    val secondId = secondQueue.submit({}) { secondCancelled += 1 }

    assertTrue(secondQueue.cancel(secondId))
    assertEquals(0, firstCancelled)
    assertEquals(1, secondCancelled)
  }

  @Test
  fun synchronousStartExceptionReportsTypedFailureBeforeTheNextOperationStarts() {
    val scheduled = ArrayDeque<() -> Unit>()
    val events = mutableListOf<String>()
    val queue = GattSerialQueue(post = { task -> scheduled.addLast(task); true })

    queue.submitCancellable(
      op = { _, _ -> throw IllegalStateException("start failed") },
      onCancelled = { events.add("cancelled") },
      onStartFailure = { error -> events.add("failure:${error.message}") }
    )
    queue.submit({ events.add("second-start") }) {}

    scheduled.removeFirst().invoke()
    assertEquals(listOf("failure:start failed"), events)
    assertEquals(0, events.count { it == "second-start" })

    scheduled.removeFirst().invoke()
    assertEquals(listOf("failure:start failed", "second-start"), events)
  }

  @Test
  fun cancellationDuringPublishedStartSettlesOnceAndWaitsForPhysicalDone() {
    val scheduled = ArrayDeque<() -> Unit>()
    val events = mutableListOf<String>()
    val queue = GattSerialQueue(post = { task -> scheduled.addLast(task); true })
    var firstId = 0L
    var firstDone: (() -> Unit)? = null

    firstId = queue.submitCancellable(
      op = { _, done ->
        events.add("first-start")
        firstDone = done
        assertTrue(queue.cancel(firstId))
        assertEquals(listOf("first-start", "first-cancelled"), events)
      },
      onCancelled = { events.add("first-cancelled") },
      onStartFailure = { error -> events.add("first-failure:${error.message}") }
    )
    queue.submit({ events.add("second-start") }) {}

    scheduled.removeFirst().invoke()
    assertEquals(listOf("first-start", "first-cancelled"), events)
    firstDone?.invoke()
    scheduled.removeFirst().invoke()
    assertEquals(listOf("first-start", "first-cancelled", "second-start"), events)
  }

  @Test
  fun scanUuidValidationRejectsMalformedValuesAndTreatsOnlyAnEmptyListAsUnfiltered() {
    assertEquals(
      listOf("0000180d-0000-1000-8000-00805f9b34fb", "12345678-0000-1000-8000-00805f9b34fb"),
      com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidGattRadio.normalizeScanServiceUuids(
        listOf("180D", "12345678")
      )
    )
    assertEquals(
      emptyList<String>(),
      com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidGattRadio.normalizeScanServiceUuids(emptyList())
    )
    var malformedRejected = false
    try {
      com.sfourdrinier.unifiedblemanager.radio.OwnedAndroidGattRadio.normalizeScanServiceUuids(
        listOf("not-a-uuid")
      )
    } catch (_: IllegalArgumentException) {
      malformedRejected = true
    }
    assertTrue(malformedRejected)
  }
}
