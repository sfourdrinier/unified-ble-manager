// android/src/test/java/com/sfourdrinier/unifiedblemanager/presence/BackgroundContinuationManifestTest.kt

package com.sfourdrinier.unifiedblemanager.presence

import android.content.SharedPreferences
import org.junit.Assert.assertEquals
import org.junit.Test
import org.mockito.Mockito.mock
import org.mockito.Mockito.`when`

/** The wake reads the build-time manifest default when the app never declared at runtime. */
class BackgroundContinuationManifestTest {
  private val manifestJson =
    "{\"onAppearance\":\"native\"," +
      "\"resubscribe\":[{\"serviceUuid\":\"0000180d-0000-1000-8000-00805f9b34fb\"," +
      "\"serviceOccurrence\":1,\"characteristicUuid\":\"00002a37-0000-1000-8000-00805f9b34fb\"," +
      "\"characteristicOccurrence\":1}]}"

  private fun preferencesWith(stored: Map<String, String?>): SharedPreferences {
    val preferences = mock(SharedPreferences::class.java)
    `when`(preferences.all).thenReturn(stored as Map<String, *>)
    return preferences
  }

  @Test
  fun manifestDefaultArmsTheWake() {
    val store = SharedPreferencesBackgroundContinuationStore(
      preferencesWith(emptyMap()),
      manifestJson = { manifestJson }
    )
    val loaded = store.loadDeclaration()
    assertEquals(ContinuationStrategy.NATIVE, loaded.strategy)
    assertEquals(1, loaded.resubscribe.size)
  }

  @Test
  fun runtimeDeclarationWinsOverManifest() {
    val store = SharedPreferencesBackgroundContinuationStore(
      preferencesWith(mapOf("background-continuation:declaration" to "{\"onAppearance\":\"record-only\",\"resubscribe\":[]}")),
      manifestJson = { manifestJson }
    )
    assertEquals(ContinuationStrategy.RECORD_ONLY, store.loadDeclaration().strategy)
  }

  @Test
  fun malformedManifestIsCountedAndFallsBackToRecordOnly() {
    val logs = mutableListOf<String>()
    val store = SharedPreferencesBackgroundContinuationStore(
      preferencesWith(emptyMap()),
      manifestJson = { "{\"onAppearance\":" },
      log = { logs.add(it) }
    )
    assertEquals(ContinuationStrategy.RECORD_ONLY, store.loadDeclaration().strategy)
    assertEquals(1L, store.malformedDeclarationCount())
    assertEquals(1, logs.size)
  }
}
