// android/src/main/java/com/sfourdrinier/unifiedblemanager/presence/BackgroundContinuationStore.kt

package com.sfourdrinier.unifiedblemanager.presence

import android.content.Context
import android.content.SharedPreferences

/**
 * The persisted standing order (BGS4): written by the JS-declared owner
 * while the app is alive, read by the OS wake with no JavaScript. Peer
 * addresses stay on device, inside the app-private store. A malformed
 * persisted record falls back to `record-only` and is counted and logged —
 * reported, never silently dropped.
 */
interface BackgroundContinuationStore {
  fun saveDeclaration(json: String)
  fun loadDeclaration(): BackgroundContinuationDeclaration
  /** Persisted declarations that could not be parsed. */
  fun malformedDeclarationCount(): Long
  fun recordWakeOutcome(outcome: ContinuationWakeRecord)
  fun lastWakeOutcome(): ContinuationWakeRecord?
}

/**
 * App-private SharedPreferences continuation store. Sources, in order: the
 * runtime declaration JS persisted at manager open, then the build-time
 * default the Expo plugin wrote to the manifest (`META_DATA_KEY`), then
 * `record-only`. A malformed record of either source falls back to
 * `record-only` counted and logged — reported, never silently dropped.
 */
class SharedPreferencesBackgroundContinuationStore(
  private val preferences: SharedPreferences,
  private val manifestJson: () -> String?,
  private val log: (String) -> Unit = { message -> android.util.Log.w(TAG, message) }
) : BackgroundContinuationStore {
  constructor(context: Context) : this(
    context.applicationContext.getSharedPreferences(
      "ubm-background-continuation",
      Context.MODE_PRIVATE
    ),
    manifestJson = { readManifestDeclaration(context.applicationContext) }
  )

  private val lock = Any()
  private var malformed = 0L

  override fun saveDeclaration(json: String) {
    // Validated at parse on every read, so a write never fails: the wake
    // refuses what it cannot parse instead of the app crashing on declare.
    synchronized(lock) {
      preferences.edit().putString(DECLARATION_KEY, json).apply()
    }
  }

  override fun loadDeclaration(): BackgroundContinuationDeclaration {
    synchronized(lock) {
      val runtime = preferences.all[DECLARATION_KEY] as? String
      if (runtime != null) return parsedOrRecordOnly(runtime, "runtime")
      val manifest = manifestJson()
      if (manifest != null) return parsedOrRecordOnly(manifest, "manifest")
      return BackgroundContinuationDeclaration.recordOnly()
    }
  }

  private fun parsedOrRecordOnly(json: String, source: String): BackgroundContinuationDeclaration {
    return try {
      BackgroundContinuationDeclaration.parse(json)
    } catch (error: IllegalArgumentException) {
      malformed += 1
      log("background continuation $source declaration unparseable, using record-only: ${error.message}")
      BackgroundContinuationDeclaration.recordOnly()
    }
  }

  override fun malformedDeclarationCount(): Long {
    synchronized(lock) {
      return malformed
    }
  }

  override fun recordWakeOutcome(outcome: ContinuationWakeRecord) {
    synchronized(lock) {
      preferences.edit()
        .putString(
          WAKE_KEY,
          "${outcome.observedAtMs}|${outcome.event}|${outcome.strategy.wire}|" +
            "${outcome.peerAddress ?: ""}|${outcome.code ?: ""}|${outcome.reason ?: ""}"
        )
        .apply()
    }
  }

  override fun lastWakeOutcome(): ContinuationWakeRecord? {
    synchronized(lock) {
      val text = preferences.all[WAKE_KEY] as? String ?: return null
      val parts = text.split('|', limit = 6)
      if (parts.size != 6) return null
      val observedAt = parts[0].toLongOrNull() ?: return null
      val strategy = ContinuationStrategy.values().firstOrNull { it.wire == parts[2] } ?: return null
      return ContinuationWakeRecord(
        observedAtMs = observedAt,
        event = parts[1],
        strategy = strategy,
        peerAddress = parts[3].ifEmpty { null },
        code = parts[4].ifEmpty { null },
        reason = parts[5].ifEmpty { null }
      )
    }
  }

  companion object {
    private const val DECLARATION_KEY = "background-continuation:declaration"
    private const val WAKE_KEY = "background-continuation:last-wake"
    private const val TAG = "UnifiedBleContinuation"

    /**
     * Manifest meta-data name the Expo plugin writes when
     * `background.continuation` is configured: the build-time default the
     * wake reads when the app never declared at runtime. A runtime declare
     * always wins; an explicit runtime `record-only` clears it.
     */
    const val MANIFEST_KEY = "com.sfourdrinier.unifiedblemanager.BACKGROUND_CONTINUATION"

    private fun readManifestDeclaration(context: Context): String? {
      return try {
        val info = context.packageManager.getApplicationInfo(context.packageName, android.content.pm.PackageManager.GET_META_DATA)
        info.metaData?.getString(MANIFEST_KEY)
      } catch (_: android.content.pm.PackageManager.NameNotFoundException) {
        null
      }
    }
  }
}

/** In-memory continuation store for tests: the same fallback-and-count shape. */
class InMemoryBackgroundContinuationStore : BackgroundContinuationStore {
  private val lock = Any()
  private var declarationJson: String? = null
  private var malformed = 0L
  private var lastWake: ContinuationWakeRecord? = null

  override fun saveDeclaration(json: String) {
    synchronized(lock) {
      declarationJson = json
    }
  }

  override fun loadDeclaration(): BackgroundContinuationDeclaration {
    synchronized(lock) {
      val json = declarationJson ?: return BackgroundContinuationDeclaration.recordOnly()
      return try {
        BackgroundContinuationDeclaration.parse(json)
      } catch (error: IllegalArgumentException) {
        malformed += 1
        BackgroundContinuationDeclaration.recordOnly()
      }
    }
  }

  override fun malformedDeclarationCount(): Long {
    synchronized(lock) {
      return malformed
    }
  }

  override fun recordWakeOutcome(outcome: ContinuationWakeRecord) {
    synchronized(lock) {
      lastWake = outcome
    }
  }

  override fun lastWakeOutcome(): ContinuationWakeRecord? {
    synchronized(lock) {
      return lastWake
    }
  }
}
