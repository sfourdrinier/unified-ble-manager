// android/src/test/java/com/sfourdrinier/unifiedblemanager/presence/BackgroundContinuationManifestTest.kt

package com.sfourdrinier.unifiedblemanager.presence

import android.content.SharedPreferences
import org.junit.Assert.assertEquals
import org.junit.Test

/** The wake reads the build-time manifest default when the app never declared at runtime. */
class BackgroundContinuationManifestTest {
  private val manifestJson =
    "{\"onAppearance\":\"native\"," +
      "\"resubscribe\":[{\"serviceUuid\":\"0000180d-0000-1000-8000-00805f9b34fb\"," +
      "\"serviceOccurrence\":1,\"characteristicUuid\":\"00002a37-0000-1000-8000-00805f9b34fb\"," +
      "\"characteristicOccurrence\":1}]}"

  /** Map-backed preferences: edits apply, so malformed counts persist like the device store. */
  private class FakePrefs(seed: Map<String, Any?> = emptyMap()) : SharedPreferences {
    private val map = LinkedHashMap<String, Any?>(seed)

    override fun getAll(): Map<String, *> = synchronized(this) { HashMap<String, Any?>(map) }
    override fun getString(key: String, defValue: String?): String? = synchronized(this) {
      map[key] as? String ?: defValue
    }
    override fun getLong(key: String, defValue: Long): Long = synchronized(this) {
      map[key] as? Long ?: defValue
    }
    override fun getInt(key: String, defValue: Int): Int = defValue
    override fun getFloat(key: String, defValue: Float): Float = defValue
    override fun getBoolean(key: String, defValue: Boolean): Boolean = defValue
    override fun getStringSet(key: String, defValues: Set<String>?): Set<String>? = defValues
    override fun contains(key: String): Boolean = synchronized(this) { map.containsKey(key) }
    override fun edit(): SharedPreferences.Editor = FakeEditor()
    override fun registerOnSharedPreferenceChangeListener(
      listener: SharedPreferences.OnSharedPreferenceChangeListener?
    ) = Unit
    override fun unregisterOnSharedPreferenceChangeListener(
      listener: SharedPreferences.OnSharedPreferenceChangeListener?
    ) = Unit

    private inner class FakeEditor : SharedPreferences.Editor {
      private val puts = HashMap<String, Any?>()
      private val removes = HashSet<String>()
      private var clear = false

      override fun putString(key: String, value: String?): SharedPreferences.Editor {
        puts[key] = value
        return this
      }
      override fun putLong(key: String, value: Long): SharedPreferences.Editor {
        puts[key] = value
        return this
      }
      override fun remove(key: String): SharedPreferences.Editor {
        removes.add(key)
        return this
      }
      override fun clear(): SharedPreferences.Editor {
        clear = true
        return this
      }
      private fun flush() = synchronized(this@FakePrefs) {
        if (clear) map.clear()
        for (key in removes) map.remove(key)
        map.putAll(puts)
      }
      override fun commit(): Boolean {
        flush()
        return true
      }
      override fun apply() = flush()
      override fun putInt(key: String, value: Int): SharedPreferences.Editor = this
      override fun putFloat(key: String, value: Float): SharedPreferences.Editor = this
      override fun putBoolean(key: String, value: Boolean): SharedPreferences.Editor = this
      override fun putStringSet(key: String, values: Set<String>?): SharedPreferences.Editor = this
    }
  }

  @Test
  fun manifestDefaultArmsTheWake() {
    val store = SharedPreferencesBackgroundContinuationStore(
      FakePrefs(),
      manifestJson = { manifestJson }
    )
    val loaded = store.loadDeclaration()
    assertEquals(ContinuationStrategy.NATIVE, loaded.strategy)
    assertEquals(1, loaded.resubscribe.size)
  }

  @Test
  fun runtimeDeclarationWinsOverManifest() {
    val store = SharedPreferencesBackgroundContinuationStore(
      FakePrefs(mapOf("background-continuation:declaration" to "{\"onAppearance\":\"record-only\",\"resubscribe\":[]}")),
      manifestJson = { manifestJson }
    )
    assertEquals(ContinuationStrategy.RECORD_ONLY, store.loadDeclaration().strategy)
  }

  @Test
  fun malformedManifestIsCountedAndFallsBackToRecordOnly() {
    val logs = mutableListOf<String>()
    val store = SharedPreferencesBackgroundContinuationStore(
      FakePrefs(),
      manifestJson = { "{\"onAppearance\":" },
      log = { logs.add(it) }
    )
    assertEquals(ContinuationStrategy.RECORD_ONLY, store.loadDeclaration().strategy)
    assertEquals(1L, store.malformedDeclarationCount())
    assertEquals(1, logs.size)
  }

  @Test
  fun rereadingOneBadPayloadCountsOnce() {
    val store = SharedPreferencesBackgroundContinuationStore(
      FakePrefs(),
      manifestJson = { "{\"onAppearance\":" },
      log = {}
    )
    store.loadDeclaration()
    store.loadDeclaration()
    assertEquals(1L, store.malformedDeclarationCount())
  }

  @Test
  fun distinctBadPayloadsCountSeparately() {
    val prefs = FakePrefs()
    val store = SharedPreferencesBackgroundContinuationStore(prefs, manifestJson = { null }, log = {})
    store.saveDeclaration("{\"onAppearance\":")
    store.loadDeclaration()
    assertEquals(1L, store.malformedDeclarationCount())
    store.saveDeclaration("not json at all")
    store.loadDeclaration()
    assertEquals(2L, store.malformedDeclarationCount())
  }

  @Test
  fun malformedCountSurvivesANewStoreInstance() {
    val prefs = FakePrefs()
    val first = SharedPreferencesBackgroundContinuationStore(prefs, manifestJson = { "{\"onAppearance\":" }, log = {})
    first.loadDeclaration()
    assertEquals(1L, first.malformedDeclarationCount())
    // A new store over the same device prefs (the woken process) still sees it.
    val second = SharedPreferencesBackgroundContinuationStore(prefs, manifestJson = { null }, log = {})
    assertEquals(1L, second.malformedDeclarationCount())
  }
}
