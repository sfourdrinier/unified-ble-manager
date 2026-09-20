// android/src/test/java/com/sfourdrinier/unifiedblemanager/presence/PresenceRestoredStoreTest.kt

package com.sfourdrinier.unifiedblemanager.presence

import android.content.SharedPreferences
import java.util.concurrent.ConcurrentHashMap
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * PresenceRestoredStore exactly-once drain (RV2 finding 1): a save that
 * lands between the drain's snapshot and its wipe must survive, and records
 * the store cannot parse must be reported, never silently dropped.
 */
class PresenceRestoredStoreTest {
  /** In-memory SharedPreferences whose snapshot hook runs after the copy,
   *  modelling a service-thread save that lands after `all` but before the
   *  drain's wipe. */
  private class FakePrefs : SharedPreferences {
    private val map = LinkedHashMap<String, Any?>()
    var onSnapshot: (() -> Unit)? = null

    fun putDirect(key: String, value: Any?) {
      synchronized(this) { map[key] = value }
    }

    override fun getAll(): Map<String, *> = synchronized(this) {
      val snapshot = HashMap<String, Any?>(map)
      onSnapshot?.invoke()
      snapshot
    }

    override fun getString(key: String, defValue: String?): String? = synchronized(this) {
      map[key] as? String ?: defValue
    }

    override fun getInt(key: String, defValue: Int): Int = defValue
    override fun getLong(key: String, defValue: Long): Long = defValue
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
      override fun putLong(key: String, value: Long): SharedPreferences.Editor = this
      override fun putFloat(key: String, value: Float): SharedPreferences.Editor = this
      override fun putBoolean(key: String, value: Boolean): SharedPreferences.Editor = this
      override fun putStringSet(key: String, values: Set<String>?): SharedPreferences.Editor = this
    }
  }

  @Test
  fun aSaveBetweenSnapshotAndWipeSurvivesTheDrain() {
    val prefs = FakePrefs()
    val logs = mutableListOf<String>()
    val store = SharedPreferencesPresenceStore(prefs, logs::add)
    store.saveAppearance("AA:BB:CC:DD:EE:01", null, 1000L)
    // The system service thread saves after the drain's `all` snapshot but
    // before its wipe: that record must survive, never be wiped unseen.
    prefs.onSnapshot = { store.saveAppearance("AA:BB:CC:DD:EE:02", 7, 2000L) }
    val first = store.drainAppearances()
    prefs.onSnapshot = null
    val second = store.drainAppearances()
    assertEquals(
      setOf(
        PresenceAppearance("AA:BB:CC:DD:EE:01", null, 1000L),
        PresenceAppearance("AA:BB:CC:DD:EE:02", 7, 2000L)
      ),
      (first + second).toSet()
    )
  }

  @Test
  fun malformedAndForeignRecordsAreReportedNeverSilentlyDropped() {
    val prefs = FakePrefs()
    val logs = mutableListOf<String>()
    val store = SharedPreferencesPresenceStore(prefs, logs::add)
    prefs.putDirect("presence-restored:AA:BB:CC:DD:EE:01", "7|2000")
    prefs.putDirect("presence-restored:AA:BB:CC:DD:EE:02", "no-separator-here")
    prefs.putDirect("presence-restored:AA:BB:CC:DD:EE:03", "7|not-a-number")
    prefs.putDirect("presence-restored:AA:BB:CC:DD:EE:04", 42)
    prefs.putDirect("something-else-entirely", "x")
    val drained = store.drainAppearances()
    assertEquals(
      listOf(PresenceAppearance("AA:BB:CC:DD:EE:01", 7, 2000L)),
      drained
    )
    assertEquals(4L, store.malformedRecordCount())
    assertEquals(4, logs.size)
    // The drain converges: nothing parseable or reported is left behind.
    assertTrue(store.drainAppearances().isEmpty())
    assertEquals(0, prefs.getAll().size)
  }

  @Test
  fun concurrentSavesAreNeverLostOrDuplicatedByDrains() {
    val store = InMemoryPresenceStore()
    val saved = ConcurrentHashMap.newKeySet<String>()
    val returned = ConcurrentHashMap.newKeySet<String>()
    val duplicates = ConcurrentHashMap.newKeySet<String>()
    val savers = (0 until 4).map { thread ->
      Thread {
        for (i in 0 until 500) {
          val address = "peer-$thread-$i"
          store.saveAppearance(address, thread, i.toLong())
          saved.add(address)
        }
      }
    }
    val drainer = Thread {
      while (savers.any { it.isAlive }) {
        for (appearance in store.drainAppearances()) {
          if (!returned.add(appearance.address)) duplicates.add(appearance.address)
        }
      }
    }
    savers.forEach { it.start() }
    drainer.start()
    savers.forEach { it.join(30_000) }
    drainer.join(30_000)
    for (appearance in store.drainAppearances()) {
      if (!returned.add(appearance.address)) duplicates.add(appearance.address)
    }
    assertTrue("duplicates: $duplicates", duplicates.isEmpty())
    assertEquals(saved, returned)
  }
}
