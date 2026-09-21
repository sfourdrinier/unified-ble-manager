// android/src/test/java/com/sfourdrinier/unifiedblemanager/presence/BackgroundContinuationStoreTest.kt

package com.sfourdrinier.unifiedblemanager.presence

import org.junit.Assert.assertEquals
import org.junit.Test

/** The declared standing order survives process death; the wake reads it with no JS (BGS4). */
class BackgroundContinuationStoreTest {
  private val nativeJson =
    "{\"onAppearance\":\"native\",\"peerId\":\"A0:9E:1A:E9:B9:3D\"," +
      "\"resubscribe\":[{\"serviceUuid\":\"0000180d-0000-1000-8000-00805f9b34fb\"," +
      "\"serviceOccurrence\":1,\"characteristicUuid\":\"00002a37-0000-1000-8000-00805f9b34fb\"," +
      "\"characteristicOccurrence\":1}]}"

  @Test
  fun emptyStoreMeansRecordOnly() {
    val store = InMemoryBackgroundContinuationStore()
    assertEquals(ContinuationStrategy.RECORD_ONLY, store.loadDeclaration().strategy)
  }

  @Test
  fun declarationRoundTrips() {
    val store = InMemoryBackgroundContinuationStore()
    store.saveDeclaration(nativeJson)
    val loaded = store.loadDeclaration()
    assertEquals(ContinuationStrategy.NATIVE, loaded.strategy)
    assertEquals("A0:9E:1A:E9:B9:3D", loaded.peerId)
    assertEquals(1, loaded.resubscribe.size)
  }

  @Test
  fun malformedPersistedJsonFallsBackToRecordOnlyAndIsCountedNeverSilent() {
    val store = InMemoryBackgroundContinuationStore()
    store.saveDeclaration("{\"onAppearance\":")
    assertEquals(ContinuationStrategy.RECORD_ONLY, store.loadDeclaration().strategy)
    assertEquals(1L, store.malformedDeclarationCount())
  }

  @Test
  fun rereadingOneBadDeclarationCountsOnce() {
    val store = InMemoryBackgroundContinuationStore()
    store.saveDeclaration("{\"onAppearance\":")
    store.loadDeclaration()
    store.loadDeclaration()
    assertEquals(1L, store.malformedDeclarationCount())
  }

  @Test
  fun distinctBadDeclarationsCountSeparately() {
    val store = InMemoryBackgroundContinuationStore()
    store.saveDeclaration("{\"onAppearance\":")
    store.loadDeclaration()
    store.saveDeclaration("not json at all")
    store.loadDeclaration()
    assertEquals(2L, store.malformedDeclarationCount())
  }

  @Test
  fun lastWakeOutcomeRoundTripsForDiagnostics() {
    val store = InMemoryBackgroundContinuationStore()
    assertEquals(null, store.lastWakeOutcome())
    val outcome = ContinuationWakeRecord(
      observedAtMs = 99L,
      event = "continuation.completed",
      strategy = ContinuationStrategy.NATIVE,
      peerAddress = "A0:9E:1A:E9:B9:3D",
      code = null,
      reason = null
    )
    store.recordWakeOutcome(outcome)
    assertEquals(outcome, store.lastWakeOutcome())
  }
}
