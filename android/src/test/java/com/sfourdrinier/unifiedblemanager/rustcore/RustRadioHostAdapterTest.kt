// android/src/test/java/com/sfourdrinier/unifiedblemanager/rustcore/RustRadioHostAdapterTest.kt

package com.sfourdrinier.unifiedblemanager.rustcore

import com.sfourdrinier.unifiedblemanager.background.ForegroundServiceControlException
import com.sfourdrinier.unifiedblemanager.radio.AndroidGattNotSubmitted
import com.sfourdrinier.unifiedblemanager.radio.AndroidGattOperationFailure
import com.sfourdrinier.unifiedblemanager.presence.PresencePort
import com.sfourdrinier.unifiedblemanager.presence.PresenceRestoredPeer
import com.ubm.core.MobileCoreBridge
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class RustRadioHostAdapterTest {
  private val core = FakeCore()
  private val radio = FakeRadio()
  private val background = FakeBackground()
  private var chooser: CompanionPort? = null
  private var presence: PresencePort? = null
  private val logs = mutableListOf<String>()
  private var radioExecutor: java.util.concurrent.Executor = DirectExecutor
  private val adapter by lazy {
    RustRadioHostAdapter(core, radio, background, { chooser }, { presence }, radioExecutor, DirectExecutor) { logs.add(it) }
  }

  private val peer = "AA:BB:CC:DD:EE:FF"
  private val NO_PHY = emptyArray<String>()
  private val heartRate = CharacteristicInstance(peer, HR_SERVICE, 0, HR_MEASUREMENT, 0)

  private fun events(): RadioPortEvents {
    adapter.statusCounts()
    return radio.events
  }

  private fun connect(requestId: Long = 1) {
    adapter.connect(requestId, peer, false, NO_PHY)
    events().onConnection(peer, true, 0)
    core.calls.clear()
    radio.calls.clear()
  }

  @Test
  fun adapterStateAnswersThePlatformFacts() {
    radio.adapter = AdapterFacts("available", "denied", "unknown", "Bluetooth scan and connect permissions are not granted.")
    adapter.adapterState(3)
    assertEquals(
      listOf("adapter:3:available/denied/unknown/Bluetooth scan and connect permissions are not granted."),
      core.calls
    )
  }

  @Test
  fun scanForwardsPlatformOptionsAndAddressFilters() {
    adapter.startScan(1, arrayOf(HR_SERVICE), arrayOf(peer), "balanced", "first-match", 0)
    assertEquals(listOf("startScan:[$HR_SERVICE]:[$peer]:1:2:false"), radio.calls)
    assertEquals(listOf("unit:1"), core.calls)
  }

  @Test
  fun scanDefaultsMatchLegacyLowLatencyAllMatchesLegacyScan() {
    adapter.startScan(1, arrayOf(), null, null, null, 1)
    assertEquals(listOf("startScan:[]:[]:2:1:true"), radio.calls)
    adapter.startScan(2, null, null, "opportunistic", "all-matches", 1)
    assertEquals("startScan:[]:[]:-1:1:true", radio.calls[1])
    adapter.startScan(3, null, null, "low-power", null, 1)
    assertEquals("startScan:[]:[]:0:1:true", radio.calls[2])
  }

  @Test
  fun scanRefusalIsClassifiedNotSwallowed() {
    radio.startScanFailure = RadioPortFailure(RadioFailureKind.ADAPTER_OFF, "Bluetooth adapter is off")
    adapter.startScan(1, null, null, null, null, -1)
    radio.startScanFailure = SecurityException("BLUETOOTH_SCAN")
    adapter.startScan(2, null, null, null, null, -1)
    adapter.startScan(3, null, null, "turbo", null, -1)
    assertEquals(listOf("failure:1:adapter-off:null", "failure:2:permission-denied:null", "failure:3:unsupported:null"), core.calls)
  }

  @Test
  fun stopScanReportsCleanupFailure() {
    adapter.stopScan(1)
    radio.stopScanFailure = IllegalStateException("stopScan threw")
    adapter.stopScan(2)
    assertEquals(listOf("unit:1", "failure:2:platform:null"), core.calls)
  }

  @Test
  fun connectCompletesFromTheOsCallbackThenIngestsTheLink() {
    adapter.connect(1, peer, true, NO_PHY)
    assertEquals(listOf("connect:$peer:true"), radio.calls)
    assertTrue(core.calls.isEmpty())
    events().onConnection(peer, true, 0)
    assertEquals(listOf("unit:1", "link:$peer:true:0"), core.calls)
  }

  @Test
  fun connectEstablishesTheLinkOnThePreferredPhys() {
    adapter.connect(1, peer, false, arrayOf("le-2m", "le-coded"))
    assertEquals(listOf("connect:$peer:false:phy=6"), radio.calls)
    events().onConnection(peer, true, 0)
    assertEquals(listOf("unit:1", "link:$peer:true:0"), core.calls)
  }

  @Test
  fun aPhyPreferenceToALiveLinkIsRefusedBeforeAnyEffect() {
    connect()
    adapter.connect(2, peer, false, arrayOf("le-2m"))
    assertTrue(radio.calls.isEmpty())
    assertEquals(listOf("failure:2:unsupported:null"), core.calls)
    assertFalse(core.failures.getValue(2).dispatched)
  }

  @Test
  fun aPhyPreferenceTheOsCannotApplyIsRefusedBeforeAnyEffect() {
    radio.connectPhySupported = false
    adapter.connect(1, peer, false, arrayOf("le-1m"))
    adapter.connect(2, peer, true, arrayOf("le-2m"))
    adapter.connect(3, peer, false, arrayOf("le-3m"))
    assertTrue(radio.calls.isEmpty())
    assertEquals(
      listOf("failure:1:unsupported:null", "failure:2:unsupported:null", "failure:3:unsupported:null"),
      core.calls
    )
    assertFalse(core.failures.getValue(1).dispatched)
  }

  @Test
  fun failedConnectAnswersGattStatusWithoutInventingALink() {
    adapter.connect(1, peer, false, NO_PHY)
    events().onConnection(peer, false, 133)
    assertEquals(listOf("failure:1:gatt-status:133"), core.calls)
  }

  /**
   * Owner decision (5.0): a connect whose link Android could not establish
   * (GATT 133, HCI 0x3E = 62) crosses to Rust with its exact status, which
   * Rust reports as `caller-decides`; the adapter never retries it.
   */
  @Test
  fun aTransientConnectFailureCrossesWithItsStatusAndIsNeverRetried() {
    for ((requestId, status) in listOf(1L to 133, 2L to 62)) {
      adapter.connect(requestId, peer, false, NO_PHY)
      events().onConnection(peer, false, status)
      assertEquals(listOf("failure:$requestId:gatt-status:$status"), core.calls)
      assertEquals(1, radio.calls.count { it.startsWith("connect:") })
      core.calls.clear()
      radio.calls.clear()
    }
  }

  @Test
  fun connectToALiveLinkAnswersWithoutASecondConnectGatt() {
    connect()
    adapter.connect(2, peer, false, NO_PHY)
    assertTrue(radio.calls.isEmpty())
    assertEquals(listOf("unit:2"), core.calls)
  }

  @Test
  fun secondConcurrentConnectIsBusy() {
    adapter.connect(1, peer, false, NO_PHY)
    adapter.connect(2, peer, false, NO_PHY)
    assertEquals(listOf("failure:2:busy:null"), core.calls)
  }

  @Test
  fun cancelPendingConnectReleasesTheGattAndReportsCancelledOnce() {
    adapter.connect(1, peer, false, NO_PHY)
    adapter.cancel(1)
    assertEquals(listOf("connect:$peer:false", "disconnect:$peer"), radio.calls)
    events().onConnection(peer, false, 0)
    assertEquals(listOf("failure:1:cancelled:null"), core.calls)
  }

  @Test
  fun cancelBeforeDispatchHasNoEffect() {
    val queued = QueuedExecutor()
    radioExecutor = queued
    adapter.connect(1, peer, false, NO_PHY)
    adapter.cancel(1)
    queued.runAll()
    assertTrue(radio.calls.isEmpty())
    assertEquals(listOf("failure:1:cancelled:null"), core.calls)
  }

  @Test
  fun cancelInFlightGattOperationCancelsExactlyThatDriverOperation() {
    connect()
    radio.characteristics[heartRate] = CharacteristicFacts(0x12, true)
    adapter.read(5, peer, HR_SERVICE, 0, HR_MEASUREMENT, 0)
    adapter.cancel(5)
    assertEquals(listOf(100L), radio.cancelled)
    assertEquals(listOf("failure:5:cancelled:null"), core.calls)
  }

  @Test
  fun cancelOfANonCancellableRequestIsCountedAndAnsweredTruthfullyLater() {
    radio.cancellable = false
    connect()
    adapter.createBond(4, peer, "le")
    adapter.cancel(4)
    assertTrue(core.calls.isEmpty())
    // The OS ceremony could not be stopped and completed: report what happened.
    radio.answer("createBond:$peer:le", Result.success(radio.security))
    assertEquals(listOf("security:4:${radio.security}"), core.calls)
    assertEquals(1L, adapter.statusCounts()["cancel-not-cancellable"])
  }

  @Test
  fun disconnectAnswersAfterTheOsAndIngestsTheLinkEnd() {
    connect()
    adapter.disconnect(2, peer)
    assertEquals(listOf("unit:2"), core.calls)
    events().onConnection(peer, false, 0)
    assertEquals(listOf("unit:2", "link:$peer:false:0"), core.calls)
  }

  @Test
  fun disconnectCleanupFailureIsReported() {
    radio.disconnectAnswer = IllegalStateException("close failed")
    adapter.disconnect(2, peer)
    assertEquals(listOf("failure:2:platform:null"), core.calls)
  }

  @Test
  fun discoveryEncodesPreOrderTreeWithDuplicateOccurrences() {
    connect()
    adapter.discover(3, peer)
    radio.answer(
      "discover:$peer",
      Result.success(
        listOf(
          GattServiceNode(HR_SERVICE, listOf(GattCharacteristicNode(HR_MEASUREMENT, 0x10, listOf(GattDescriptorNode(CCCD))))),
          GattServiceNode(
            CUSTOM,
            listOf(GattCharacteristicNode(CUSTOM, 0x0a, emptyList()), GattCharacteristicNode(CUSTOM, 0x04, emptyList()))
          ),
          GattServiceNode(CUSTOM, emptyList())
        )
      )
    )
    assertEquals(
      listOf(
        "discovered:3:0/$HR_SERVICE/0/0,1/$HR_MEASUREMENT/0/16,2/$CCCD/0/0," +
          "0/$CUSTOM/0/0,1/$CUSTOM/0/10,1/$CUSTOM/1/4,0/$CUSTOM/1/0"
      ),
      core.calls
    )
  }

  @Test
  fun gattOperationsBeforeConnectAreNotConnected() {
    adapter.read(1, peer, HR_SERVICE, 0, HR_MEASUREMENT, 0)
    adapter.discover(2, peer)
    adapter.readRssi(3, peer)
    adapter.requestConnectionPriority(4, peer, "high-throughput")
    assertEquals(
      listOf("failure:1:not-connected:null", "failure:2:not-connected:null", "failure:3:not-connected:null", "failure:4:not-connected:null"),
      core.calls
    )
    assertTrue(radio.calls.isEmpty())
  }

  @Test
  fun missingPathIsStale() {
    connect()
    adapter.read(1, peer, HR_SERVICE, 1, HR_MEASUREMENT, 0)
    assertEquals(listOf("failure:1:path-stale:null"), core.calls)
  }

  @Test
  fun readWriteAndGattStatusMapping() {
    connect()
    radio.characteristics[heartRate] = CharacteristicFacts(0x1a, true)
    adapter.read(1, peer, HR_SERVICE, 0, HR_MEASUREMENT, 0)
    radio.answer("read:$HR_MEASUREMENT", Result.success(byteArrayOf(0x00, 0x48)))
    val value = byteArrayOf(1, 2)
    adapter.write(2, peer, HR_SERVICE, 0, HR_MEASUREMENT, 0, value, false)
    value[0] = 9
    radio.answer("write:$HR_MEASUREMENT:[1, 2]:false", Result.failure(AndroidGattOperationFailure("characteristic-write", 5)))
    adapter.write(3, peer, HR_SERVICE, 0, HR_MEASUREMENT, 0, byteArrayOf(3), true)
    radio.answer("write:$HR_MEASUREMENT:[3]:true", Result.failure(AndroidGattOperationFailure("characteristic-write", 19)))
    adapter.write(4, peer, HR_SERVICE, 0, HR_MEASUREMENT, 0, byteArrayOf(4), true)
    radio.answer("write:$HR_MEASUREMENT:[4]:true", Result.success(Unit))
    assertEquals(
      listOf("read:1:[0, 72]:read-response", "failure:2:gatt-status:5", "failure:3:not-connected:19", "unit:4"),
      core.calls
    )
  }

  @Test
  fun writeLimitsBeforeAnyMtuExchangeUseTheAttDefaultAndStackLongWrites() {
    connect()
    adapter.readWriteLimits(1, peer)
    radio.answer("readMtu:$peer", Result.success(null))
    adapter.readWriteLimits(2, peer)
    radio.answer("readMtu:$peer", Result.success(247))
    adapter.readWriteLimits(3, peer)
    radio.answer("readMtu:$peer", Result.failure(IllegalStateException("Not connected to $peer")))
    adapter.readWriteLimits(4, peer)
    radio.answer("readMtu:$peer", Result.success(517))
    assertEquals(
      listOf("writeLimits:1:512:20", "writeLimits:2:512:244", "writeLimits:4:512:512"),
      core.calls.filter { it.startsWith("writeLimits:") }
    )
    assertTrue("a failed MTU read withholds the limits", core.failures.containsKey(3))
  }

  @Test
  fun writeCommitDispatchFollowsWhatTheStackAccepted() {
    connect()
    radio.characteristics[heartRate] = CharacteristicFacts(0x0c, true)
    // Refused synchronously by BluetoothGatt (API 33 status 201 = write request busy).
    adapter.write(1, peer, HR_SERVICE, 0, HR_MEASUREMENT, 0, byteArrayOf(1), false)
    radio.answer(
      "write:$HR_MEASUREMENT:[1]:false",
      Result.failure(AndroidGattOperationFailure("characteristic-write", 201, submitted = false))
    )
    // Legacy boolean API refused to start.
    adapter.write(2, peer, HR_SERVICE, 0, HR_MEASUREMENT, 0, byteArrayOf(2), true)
    radio.answer("write:$HR_MEASUREMENT:[2]:true", Result.failure(AndroidGattNotSubmitted("writeCharacteristic failed to start")))
    // Oversize value rejected by the platform before submission.
    adapter.write(3, peer, HR_SERVICE, 0, HR_MEASUREMENT, 0, byteArrayOf(3), true)
    radio.answer("write:$HR_MEASUREMENT:[3]:true", Result.failure(IllegalArgumentException("value too long")))
    // The ATT response reported an error: the write reached the peer.
    adapter.write(4, peer, HR_SERVICE, 0, HR_MEASUREMENT, 0, byteArrayOf(4), true)
    radio.answer("write:$HR_MEASUREMENT:[4]:true", Result.failure(AndroidGattOperationFailure("characteristic-write", 3)))
    // Pre-check refusal: stale path, never submitted.
    adapter.write(5, peer, HR_SERVICE, 9, HR_MEASUREMENT, 0, byteArrayOf(5), true)
    assertEquals(RadioFailureKind.BUSY, core.failures.getValue(1).kind)
    assertEquals(listOf(false, false, false, true, false), (1L..5L).map { core.failures.getValue(it).dispatched })
  }

  @Test
  fun adapterLossUsesTheExactLegacyCodes() {
    adapter.connect(1, peer, false, NO_PHY)
    events().onAdapterState(AdapterFacts("available", "granted", "resetting", null))
    adapter.connect(2, peer, false, NO_PHY)
    events().onAdapterState(AdapterFacts("available", "restricted", "unknown", null))
    adapter.connect(3, peer, false, NO_PHY)
    events().onAdapterState(AdapterFacts("available", "not-determined", "unknown", null))
    adapter.connect(4, peer, false, NO_PHY)
    events().onAdapterState(AdapterFacts("unavailable", "granted", "unknown", "gone"))
    assertEquals(
      listOf("adapter-resetting", "permission-restricted", "permission-not-determined", "adapter-unavailable"),
      (1L..4L).map { core.failures.getValue(it).kind.wire }
    )
  }

  @Test
  fun descriptorReadWrite() {
    connect()
    radio.characteristics[heartRate] = CharacteristicFacts(0x10, true)
    adapter.readDescriptor(1, peer, HR_SERVICE, 0, HR_MEASUREMENT, 0, CCCD, 0)
    radio.answer("readDescriptor:$CCCD#0", Result.success(byteArrayOf(1, 0)))
    adapter.writeDescriptor(2, peer, HR_SERVICE, 0, HR_MEASUREMENT, 0, USER_DESCRIPTION, 0, byteArrayOf(65))
    radio.answer("writeDescriptor:$USER_DESCRIPTION:[65]", Result.success(Unit))
    assertEquals(listOf("bytes:1:[1, 0]", "unit:2"), core.calls)
  }

  @Test
  fun requiredModeWithoutThePropertyIsRefusedBeforeAnyEffect() {
    connect()
    radio.characteristics[heartRate] = CharacteristicFacts(0x10, true)
    adapter.enableNotifications(1, peer, HR_SERVICE, 0, HR_MEASUREMENT, 0, 11, "indication", null)
    assertEquals(listOf("failure:1:unsupported:null"), core.calls)
    assertTrue(radio.calls.isEmpty())
  }

  @Test
  fun enableReportsTheCccdModeActuallyWritten() {
    connect()
    radio.characteristics[heartRate] = CharacteristicFacts(0x30, true)
    adapter.enableNotifications(1, peer, HR_SERVICE, 0, HR_MEASUREMENT, 0, 11, null, "indication")
    radio.answer("notify:$HR_MEASUREMENT:true:indication", Result.success(Unit))
    adapter.enableNotifications(2, peer, HR_SERVICE, 0, HR_MEASUREMENT, 0, 12, "notification", null)
    radio.answer("notify:$HR_MEASUREMENT:true:notification", Result.success(Unit))
    assertEquals(listOf("notify:1:indication", "notify:2:notification"), core.calls)
  }

  @Test
  fun preferenceFallsBackToTheModeTheCharacteristicSupports() {
    assertEquals("indication", RustRadioHostAdapter.resolveCccdMode(null, null, 0x20))
    assertEquals("notification", RustRadioHostAdapter.resolveCccdMode(null, "indication", 0x10))
    assertEquals("notification", RustRadioHostAdapter.resolveCccdMode(null, null, 0x30))
    assertEquals("indication", RustRadioHostAdapter.resolveCccdMode("indication", "notification", 0x30))
  }

  @Test
  fun characteristicWithoutCccdReportsUnknownDelivery() {
    connect()
    radio.characteristics[heartRate] = CharacteristicFacts(0x10, false)
    adapter.enableNotifications(1, peer, HR_SERVICE, 0, HR_MEASUREMENT, 0, 11, null, null)
    radio.answer("notify:$HR_MEASUREMENT:true:notification", Result.success(Unit))
    assertEquals(listOf("notify:1:unknown"), core.calls)
  }

  @Test
  fun notificationsCarryTheEnableEpochAndStrayValuesAreSurfaced() {
    connect()
    radio.characteristics[heartRate] = CharacteristicFacts(0x10, true)
    adapter.enableNotifications(1, peer, HR_SERVICE, 0, HR_MEASUREMENT, 0, 42, null, null)
    // A value Android staged while the CCCD write was in flight.
    events().onNotification(heartRate.copy(peerId = peer.lowercase(), characteristicUuid = HR_MEASUREMENT.uppercase()), byteArrayOf(0, 60))
    radio.answer("notify:$HR_MEASUREMENT:true:notification", Result.success(Unit))
    events().onNotification(heartRate, byteArrayOf(0, 61))
    adapter.disableNotifications(2, peer, HR_SERVICE, 0, HR_MEASUREMENT, 0)
    radio.answer("notify:$HR_MEASUREMENT:false:null", Result.success(Unit))
    events().onNotification(heartRate, byteArrayOf(0, 62))
    assertEquals(
      listOf(
        "value:$peer:$HR_MEASUREMENT#0:42:[0, 60]",
        "notify:1:notification",
        "value:$peer:$HR_MEASUREMENT#0:42:[0, 61]",
        "unit:2",
        "dropped:notification"
      ),
      core.calls
    )
  }

  @Test
  fun failedEnableForgetsItsEpoch() {
    connect()
    radio.characteristics[heartRate] = CharacteristicFacts(0x10, true)
    adapter.enableNotifications(1, peer, HR_SERVICE, 0, HR_MEASUREMENT, 0, 42, null, null)
    radio.answer("notify:$HR_MEASUREMENT:true:notification", Result.failure(AndroidGattOperationFailure("cccd-write", 3)))
    events().onNotification(heartRate, byteArrayOf(1))
    assertEquals(listOf("failure:1:gatt-status:3", "dropped:notification"), core.calls)
  }

  @Test
  fun linkLossAndServicesChangedEndEnablements() {
    connect()
    radio.characteristics[heartRate] = CharacteristicFacts(0x10, true)
    adapter.enableNotifications(1, peer, HR_SERVICE, 0, HR_MEASUREMENT, 0, 42, null, null)
    radio.answer("notify:$HR_MEASUREMENT:true:notification", Result.success(Unit))
    events().onServicesChanged(peer)
    events().onNotification(heartRate, byteArrayOf(1))
    events().onConnection(peer, false, 8)
    assertEquals(
      listOf("notify:1:notification", "services-changed:$peer", "dropped:notification", "link:$peer:false:8"),
      core.calls
    )
  }

  @Test
  fun linkQualityVerbs() {
    connect()
    adapter.readRssi(1, peer)
    radio.answer("readRssi:$peer", Result.success(-58))
    adapter.readMtu(2, peer)
    radio.answer("readMtu:$peer", Result.success(null))
    adapter.requestMtu(3, peer, 247)
    radio.answer("requestMtu:$peer:247", Result.success(232))
    adapter.requestConnectionPriority(4, peer, "high-throughput")
    radio.answer("priority:$peer:high-throughput", Result.success(true))
    adapter.readPhy(5, peer)
    radio.answer("readPhy:$peer", Result.success(PhyFacts("le-2m", "le-1m")))
    adapter.requestPhy(6, peer, "le-2m", null)
    radio.answer("requestPhy:$peer:le-2m:null", Result.success(PhyFacts("le-2m", "le-2m")))
    adapter.requestPhy(7, peer, null, "le-coded")
    radio.answer("requestPhy:$peer:null:le-coded", Result.success(null))
    assertEquals(
      listOf(
        "rssi:1:-58",
        "mtu:2:0",
        "mtu:3:232",
        "accepted:4:true",
        "phy:5:le-2m/le-1m",
        "phy-request:6:true:le-2m/le-2m",
        "phy-request:7:false:null/null"
      ),
      core.calls
    )
  }

  @Test
  fun securityBondingAndBondedPeers() {
    adapter.securityState(1, peer)
    adapter.createBond(2, peer, "auto")
    radio.answer("createBond:$peer:auto", Result.success(radio.security.copy(bond = "not-bonded")))
    adapter.cancelBond(3, peer)
    adapter.bondedPeers(4)
    assertEquals(
      listOf(
        "security:1:${radio.security}",
        "security:2:${radio.security.copy(bond = "not-bonded")}",
        "failure:3:unsupported:null",
        "bonded:4:[AA:BB:CC:DD:EE:FF, 11:22:33:44:55:66]:[Polar H10, null]"
      ),
      core.calls
    )
    events().onSecurity(peer, radio.security)
    assertEquals("security-changed:$peer:bonded", core.calls.last())
  }

  @Test
  fun backgroundLeasesDriveTheForegroundServiceRegistry() {
    adapter.acquireBackground(1, "connected-device", "Polar H10 stream")
    adapter.updateBackgroundNotification(2, "background-1", "Recording", null)
    adapter.releaseBackground(3, "background-1")
    background.failure = ForegroundServiceControlException("foregroundServicePermissionDenied", "no POST_NOTIFICATIONS")
    adapter.acquireBackground(4, "connected-device", "again")
    background.failure = ForegroundServiceControlException("invalidBackgroundLease", "stale")
    adapter.releaseBackground(5, "background-9")
    assertEquals(
      listOf(
        "acquire:connected-device:Polar H10 stream",
        "update:background-1:Recording:null",
        "release:background-1",
        "acquire:connected-device:again",
        "release:background-9"
      ),
      background.calls
    )
    assertEquals(
      listOf("lease:1:background-1", "unit:2", "unit:3", "failure:4:permission-denied:null", "failure:5:platform:null"),
      core.calls
    )
  }

  @Test
  fun phyVerbsBelowApi26AreUnsupportedBeforeAnyRadioCall() {
    connect()
    radio.connectPhySupported = false
    adapter.readPhy(2, peer)
    adapter.requestPhy(3, peer, "le-2m", null)
    assertEquals(RadioFailureKind.UNSUPPORTED, core.failures.getValue(2).kind)
    assertEquals(RadioFailureKind.UNSUPPORTED, core.failures.getValue(3).kind)
    assertTrue(radio.calls.none { it.startsWith("readPhy") || it.startsWith("requestPhy") })
  }

  @Test
  fun backgroundFailuresCarryTheRegistryCodeLegacyExpoMapped() {
    background.failure = ForegroundServiceControlException("foregroundServiceNotConfigured", "Rebuild with metadata.")
    adapter.acquireBackground(1, "connected-device", "workout")
    background.failure = ForegroundServiceControlException("invalidBackgroundLease", "stale")
    adapter.releaseBackground(2, "background-9")
    assertEquals(RadioFailureKind.UNSUPPORTED, core.failures.getValue(1).kind)
    assertEquals("foregroundServiceNotConfigured", core.failures.getValue(1).nativeCode)
    assertEquals("Rebuild with metadata.", core.failures.getValue(1).detail)
    assertEquals("invalidBackgroundLease", core.failures.getValue(2).nativeCode)
  }

  @Test
  fun companionAssociationUsesTheAttachedChooser() {
    adapter.associateCompanion(1, "Polar", null)
    chooser = object : CompanionPort {
      override fun associate(name: String?, serviceUuid: String?, onResult: (Result<CompanionAssociation>) -> Unit) {
        onResult(Result.success(CompanionAssociation(17, peer, "Polar H10 $name")))
      }
    }
    adapter.associateCompanion(2, "Polar", HR_SERVICE)
    assertEquals(listOf("failure:1:unsupported:null", "companion:2:17:$peer:Polar H10 Polar"), core.calls)
  }

  @Test
  fun presenceObservationArmsAndDisarmsOneAssociatedPeer() {
    val armed = mutableListOf<String>()
    val disarmed = mutableListOf<String>()
    presence = object : PresencePort {
      override fun observe(peerId: String, onResult: (Result<Unit>) -> Unit) {
        armed.add(peerId)
        onResult(Result.success(Unit))
      }
      override fun unobserve(peerId: String, onResult: (Result<Unit>) -> Unit) {
        disarmed.add(peerId)
        onResult(Result.success(Unit))
      }
    }
    adapter.observePresence(1, peer)
    adapter.unobservePresence(2, peer)
    assertEquals(listOf(peer), armed)
    assertEquals(listOf(peer), disarmed)
    assertEquals(listOf("unit:1", "unit:2"), core.calls)
  }

  @Test
  fun presenceWithoutAnAttachedPortIsUnsupported() {
    adapter.observePresence(1, peer)
    adapter.unobservePresence(2, peer)
    assertEquals(listOf("failure:1:unsupported:null", "failure:2:unsupported:null"), core.calls)
  }

  @Test
  fun presenceRefusalIsClassifiedNotSwallowed() {
    presence = object : PresencePort {
      override fun observe(peerId: String, onResult: (Result<Unit>) -> Unit) {
        onResult(Result.failure(RadioPortFailure(RadioFailureKind.PLATFORM, "gone", nativeCode = "deviceNotAssociated")))
      }
      override fun unobserve(peerId: String, onResult: (Result<Unit>) -> Unit) {
        throw RadioPortFailure(RadioFailureKind.UNSUPPORTED, "old")
      }
    }
    adapter.observePresence(1, peer)
    adapter.unobservePresence(2, peer)
    assertEquals(listOf("failure:1:platform:null", "failure:2:unsupported:null"), core.calls)
    assertEquals("deviceNotAssociated", core.failures.getValue(1).nativeCode)
  }

  @Test
  fun presenceRestoredPeersAreIngestedIntoTheLiveOwner() {
    assertTrue(adapter.ingestPresenceRestored(listOf(PresenceRestoredPeer(peer, null, false))))
    assertEquals(listOf("restored:$peer:false"), core.calls)
    core.ingressStatus = MobileCoreBridge.STATUS_CLOSED
    assertTrue(!adapter.ingestPresenceRestored(listOf(PresenceRestoredPeer(peer, null, false))))
  }

  @Test
  fun adapterLossEndsLinksAndPendingConnects() {
    connect()
    adapter.connect(2, "11:22:33:44:55:66", false, NO_PHY)
    events().onAdapterState(AdapterFacts("available", "granted", "off", null))
    assertEquals(
      listOf("adapter-state:off", "failure:2:adapter-off:null", "link:$peer:false:null"),
      core.calls
    )
  }

  @Test
  fun scanFailureReleasesDriverOwnershipAndIsIngested() {
    events().onScanFailed(2)
    assertEquals(listOf("stopScan"), radio.calls)
    assertEquals(listOf("scan-failed:scan failed code=2"), core.calls)
  }

  @Test
  fun advertisementsAndDroppedFactsAreIngested() {
    events().onAdvertisement(
      AdvertisementFacts(peer, peer, "Polar H10", -60, null, listOf(HR_SERVICE), listOf(0x006b to byteArrayOf(1)), emptyList(), true, null)
    )
    events().onDropped("notification", "orphan")
    assertEquals(listOf("adv:$peer", "dropped:notification"), core.calls)
  }

  @Test
  fun advertisementAppearanceAndRawRecordReachTheCore() {
    val raw = byteArrayOf(2, 1, 6)
    events().onAdvertisement(
      AdvertisementFacts(peer, peer, null, -60, null, emptyList(), emptyList(), emptyList(), null, null, 0x0341, raw)
    )
    val ingested = core.advertisements.single()
    assertEquals(0x0341, ingested.appearance)
    assertEquals(listOf<Byte>(2, 1, 6), ingested.rawRecord?.toList())
  }

  @Test
  fun closeDisablesEveryLiveNotificationAndNamesFailures() {
    connect()
    val other = heartRate.copy(characteristicUuid = CUSTOM)
    radio.characteristics[heartRate] = CharacteristicFacts(0x10, true)
    radio.characteristics[other] = CharacteristicFacts(0x10, true)
    adapter.enableNotifications(1, peer, HR_SERVICE, 0, HR_MEASUREMENT, 0, 1, null, null)
    radio.answer("notify:$HR_MEASUREMENT:true:notification", Result.success(Unit))
    adapter.enableNotifications(2, peer, HR_SERVICE, 0, CUSTOM, 0, 2, null, null)
    radio.answer("notify:$CUSTOM:true:notification", Result.success(Unit))
    adapter.close(3)
    radio.answer("notify:$HR_MEASUREMENT:false:null", Result.success(Unit))
    radio.answer("notify:$CUSTOM:false:null", Result.failure(IllegalStateException("rollback rejected")))
    assertEquals("closed:3:[$CUSTOM=rollback rejected]", core.calls.last())
    // The failed scope stays live: a retried close disables it again.
    adapter.close(4)
    assertTrue(radio.pending.containsKey("notify:$CUSTOM:false:null"))
    assertFalse(radio.pending.containsKey("notify:$HR_MEASUREMENT:false:null"))
  }

  @Test
  fun closeWithNothingLiveAnswersAtOnce() {
    adapter.close(9)
    assertEquals(listOf("closed:9:[]"), core.calls)
  }

  @Test
  fun hostStatusesAreCountedAndLoggedNeverDropped() {
    core.status = MobileCoreBridge.STATUS_LATE
    adapter.stopScan(1)
    core.status = MobileCoreBridge.STATUS_CLOSED
    adapter.stopScan(2)
    core.ingressStatus = MobileCoreBridge.STATUS_CLOSED
    events().onServicesChanged(peer)
    core.ingressStatus = MobileCoreBridge.STATUS_DROPPED_ADVERTISEMENT
    events().onAdvertisement(AdvertisementFacts(peer, peer, null, null, null, emptyList(), emptyList(), emptyList(), null, null))
    val counts = adapter.statusCounts()
    assertEquals(1L, counts["complete:unit:late"])
    assertEquals(1L, counts["complete:unit:closed"])
    assertEquals(1L, counts["ingest:services-changed:closed"])
    assertEquals(1L, counts["ingest:advertisement:dropped-advertisement"])
    assertTrue(logs.any { it.contains("late") })
    assertTrue(logs.any { it.contains("services-changed") })
  }

  @Test
  fun eachRequestIsAnsweredExactlyOnce() {
    connect()
    radio.characteristics[heartRate] = CharacteristicFacts(0x10, true)
    adapter.read(1, peer, HR_SERVICE, 0, HR_MEASUREMENT, 0)
    val parked = radio.pending.getValue("read:$HR_MEASUREMENT")
    parked(Result.success(byteArrayOf(1)))
    parked(Result.success(byteArrayOf(2)))
    assertEquals(listOf("read:1:[1]:read-response"), core.calls)
    assertEquals(1L, adapter.statusCounts()["suppressed-second-answer"])
  }

  @Test
  fun cancelAfterAnswerIsANoOp() {
    adapter.stopScan(1)
    adapter.cancel(1)
    assertFalse(radio.calls.contains("disconnect:$peer"))
    assertEquals(listOf("unit:1"), core.calls)
    assertEquals(1L, adapter.statusCounts()["cancel-after-answer"])
  }

  @Test
  fun unclassifiedFailuresArePlatformWithDetail() {
    val failure = RustRadioHostAdapter.classify(IllegalStateException("boom"))
    assertEquals(RadioFailure(RadioFailureKind.PLATFORM, null, "boom"), failure)
    assertEquals(RadioFailureKind.UNSUPPORTED, RustRadioHostAdapter.classify(UnsupportedOperationException("x")).kind)
  }

  private companion object {
    const val HR_SERVICE = "0000180d-0000-1000-8000-00805f9b34fb"
    const val HR_MEASUREMENT = "00002a37-0000-1000-8000-00805f9b34fb"
    const val CCCD = "00002902-0000-1000-8000-00805f9b34fb"
    const val USER_DESCRIPTION = "00002901-0000-1000-8000-00805f9b34fb"
    const val CUSTOM = "6e400001-b5a3-f393-e0a9-e50e24dcca9e"
  }
}
