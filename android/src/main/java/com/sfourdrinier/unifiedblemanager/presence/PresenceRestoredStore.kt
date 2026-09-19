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
 */
interface PresenceRestoredStore {
  fun saveAppearance(address: String, associationId: Int?, observedAtMs: Long)
  fun removeAppearance(address: String)
  /** Takes every persisted appearance, leaving the store empty. */
  fun drainAppearances(): List<PresenceAppearance>
}

/** App-private SharedPreferences presence store. */
class SharedPreferencesPresenceStore(context: Context) : PresenceRestoredStore {
  private val preferences: SharedPreferences = context.applicationContext.getSharedPreferences(
    "ubm-presence-restored",
    Context.MODE_PRIVATE
  )

  override fun saveAppearance(address: String, associationId: Int?, observedAtMs: Long) {
    preferences.edit()
      .putString(key(address), "${associationId ?: ""}|$observedAtMs")
      .apply()
  }

  override fun removeAppearance(address: String) {
    preferences.edit().remove(key(address)).apply()
  }

  override fun drainAppearances(): List<PresenceAppearance> {
    val all = preferences.all.toMap()
    if (all.isEmpty()) return emptyList()
    val appearances = all.mapNotNull { (stored, value) ->
      val address = stored.removePrefix(PREFIX).takeIf { stored.startsWith(PREFIX) } ?: return@mapNotNull null
      val text = value as? String ?: return@mapNotNull null
      val separator = text.indexOf('|')
      if (separator < 0) return@mapNotNull null
      val association = text.substring(0, separator).toIntOrNull()
      val observedAt = text.substring(separator + 1).toLongOrNull() ?: return@mapNotNull null
      PresenceAppearance(address, association, observedAt)
    }
    preferences.edit().clear().apply()
    return appearances
  }

  companion object {
    private const val PREFIX = "presence-restored:"

    private fun key(address: String) = "$PREFIX$address"
  }
}

/** In-memory presence store for tests. */
class InMemoryPresenceStore : PresenceRestoredStore {
  private val appearances = LinkedHashMap<String, PresenceAppearance>()

  override fun saveAppearance(address: String, associationId: Int?, observedAtMs: Long) {
    appearances[address] = PresenceAppearance(address, associationId, observedAtMs)
  }

  override fun removeAppearance(address: String) {
    appearances.remove(address)
  }

  override fun drainAppearances(): List<PresenceAppearance> {
    val taken = appearances.values.toList()
    appearances.clear()
    return taken
  }
}
