// android/src/main/java/com/sfourdrinier/unifiedblemanager/presence/PresenceRestoredStore.kt

package com.sfourdrinier.unifiedblemanager.presence

import android.content.Context
import android.content.SharedPreferences

/** One peer the system reported present while no session was alive to ingest it. */
data class PresenceAppearance(val address: String, val associationId: Int?, val observedAtMs: Long)

/**
 * The on-device presence appearance record (issue #212): written by
 * [UbmCompanionPresenceService] when the process wakes with no live session,
 * drained exactly once at the next session open. Peer addresses stay on
 * device, inside the app-private store.
 *
 * Saves arrive on the system service thread ([PresenceWakeCoordinator])
 * while the drain runs on the RN module executor, so every access shares
 * one lock: the drain removes exactly the keys it snapshotted, and a save
 * that lands after the snapshot is never wiped unseen.
 */
interface PresenceRestoredStore {
  fun saveAppearance(address: String, associationId: Int?, observedAtMs: Long)
  fun removeAppearance(address: String)
  /** Takes every persisted appearance, leaving the store empty. */
  fun drainAppearances(): List<PresenceAppearance>
  /** Records the store could not parse and reported instead of restoring. */
  fun malformedRecordCount(): Long
}

/** App-private SharedPreferences presence store. */
class SharedPreferencesPresenceStore(
  private val preferences: SharedPreferences,
  private val log: (String) -> Unit = { message -> android.util.Log.w(TAG, message) }
) : PresenceRestoredStore {
  constructor(context: Context) : this(
    context.applicationContext.getSharedPreferences(
      "ubm-presence-restored",
      Context.MODE_PRIVATE
    )
  )

  private val lock = Any()
  private var malformed = 0L

  override fun saveAppearance(address: String, associationId: Int?, observedAtMs: Long) {
    synchronized(lock) {
      preferences.edit()
        .putString(key(address), "${associationId ?: ""}|$observedAtMs")
        .apply()
    }
  }

  override fun removeAppearance(address: String) {
    synchronized(lock) {
      preferences.edit().remove(key(address)).apply()
    }
  }

  override fun drainAppearances(): List<PresenceAppearance> {
    synchronized(lock) {
      val all = preferences.all.toMap()
      if (all.isEmpty()) return emptyList()
      val appearances = ArrayList<PresenceAppearance>(all.size)
      for ((stored, value) in all) {
        appearances.add(parseAppearance(stored, value) ?: continue)
      }
      // Exactly the snapshotted keys: a save that landed after `all` is a
      // key this drain never saw, so it survives for the next drain instead
      // of being wiped without ever being returned.
      val editor = preferences.edit()
      for (stored in all.keys) editor.remove(stored)
      editor.apply()
      return appearances
    }
  }

  override fun malformedRecordCount(): Long {
    synchronized(lock) {
      return malformed
    }
  }

  /** Parses one snapshotted record, reporting (log + counter) what cannot
   *  be restored instead of dropping it silently. */
  private fun parseAppearance(stored: String, value: Any?): PresenceAppearance? {
    if (!stored.startsWith(PREFIX)) {
      reportMalformed("presence store dropped a record with an unexpected key")
      return null
    }
    val text = value as? String
    if (text == null) {
      reportMalformed("presence store dropped an unparseable appearance record")
      return null
    }
    val separator = text.indexOf('|')
    if (separator < 0) {
      reportMalformed("presence store dropped an unparseable appearance record")
      return null
    }
    val observedAt = text.substring(separator + 1).toLongOrNull()
    if (observedAt == null) {
      reportMalformed("presence store dropped an unparseable appearance record")
      return null
    }
    val association = text.substring(0, separator).toIntOrNull()
    return PresenceAppearance(stored.removePrefix(PREFIX), association, observedAt)
  }

  private fun reportMalformed(message: String) {
    malformed += 1
    log(message)
  }

  companion object {
    private const val PREFIX = "presence-restored:"
    private const val TAG = "UnifiedBlePresence"

    private fun key(address: String) = "$PREFIX$address"
  }
}

/** In-memory presence store for tests: the same lock-and-remove-drained
 *  shape as the SharedPreferences store, so the double cannot hide a race
 *  the production store has (or vice versa). Typed values cannot be
 *  malformed, so the counter stays zero. */
class InMemoryPresenceStore : PresenceRestoredStore {
  private val lock = Any()
  private val appearances = LinkedHashMap<String, PresenceAppearance>()

  override fun saveAppearance(address: String, associationId: Int?, observedAtMs: Long) {
    synchronized(lock) {
      appearances[address] = PresenceAppearance(address, associationId, observedAtMs)
    }
  }

  override fun removeAppearance(address: String) {
    synchronized(lock) {
      appearances.remove(address)
    }
  }

  override fun drainAppearances(): List<PresenceAppearance> {
    synchronized(lock) {
      val taken = appearances.values.toList()
      for (appearance in taken) appearances.remove(appearance.address)
      return taken
    }
  }

  override fun malformedRecordCount(): Long = 0L
}
