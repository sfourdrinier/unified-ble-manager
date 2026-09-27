package com.sfourdrinier.unifiedblemanager.presence

import android.app.Service
import android.content.Context
import android.content.Intent
import android.os.IBinder
import android.os.PowerManager
import com.facebook.react.ReactApplication
import com.facebook.react.bridge.Arguments
import com.facebook.react.bridge.UiThreadUtil
import com.facebook.react.jstasks.HeadlessJsTaskConfig
import com.facebook.react.jstasks.HeadlessJsTaskContext
import com.facebook.react.jstasks.HeadlessJsTaskEventListener
import java.util.UUID
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit

/** Explicit, bounded RN task dispatch. JS task success is owned by the application. */
class UbmHeadlessContinuationService : Service(), HeadlessJsTaskEventListener {
  private val worker = Executors.newSingleThreadExecutor { runnable -> Thread(runnable, "ubm-headless-start") }
  private val starting = mutableSetOf<Request>()
  private val tasks = mutableMapOf<Int, HeadlessJsTaskContext>()
  private var wakeLock: PowerManager.WakeLock? = null
  private var destroyed = false

  override fun onBind(intent: Intent?): IBinder? = null

  override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
    val request = intent?.getStringExtra(REQUEST_KEY)?.let { pending.remove(it) }
    if (request == null) { stopIfIdle(); return START_NOT_STICKY }
    if (starting.size + tasks.size >= 16) {
      request.admission.cancel("lifecycle.invalid-state", "Headless service task capacity is full")
      return START_NOT_STICKY
    }
    starting.add(request)
    try {
      val lock = wakeLock ?: (getSystemService(Context.POWER_SERVICE) as PowerManager)
        .newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "unified-ble-manager:headless").also {
          it.setReferenceCounted(false); wakeLock = it
        }
      lock.acquire(TASK_TIMEOUT_MS + START_TIMEOUT_MS)
      val host = (application as? ReactApplication)?.reactHost
        ?: throw IllegalStateException("The application must expose its React Native ReactHost")
      val startup = host.start()
      worker.execute {
        try {
          if (!startup.waitForCompletion(START_TIMEOUT_MS, TimeUnit.MILLISECONDS)) {
            request.admission.cancel("operation.timed-out", "ReactHost cold start exceeded its deadline")
          } else if (startup.isCancelled()) {
            request.admission.cancel("operation.aborted", "ReactHost cold start was cancelled")
          } else if (startup.isFaulted()) {
            val cause = startup.getError() ?: IllegalStateException("ReactHost startup failed without a cause")
            request.admission.dispatch { continuationPlatformFailure(ContinuationStrategy.HEADLESS_TASK, cause) }
          }
          UiThreadUtil.runOnUiThread {
            try {
              if (destroyed) request.admission.cancel("operation.aborted", "Headless service was destroyed during startup")
              request.admission.dispatch {
                val context = host.currentReactContext
                  ?: throw IllegalStateException("ReactHost completed startup without a ReactContext")
                check(context.hasActiveReactInstance()) { "ReactHost has no active JavaScript instance" }
                val owner = HeadlessJsTaskContext.getInstance(context)
                check(tasks.values.all { it === owner }) { "Previous ReactContext still owns active headless tasks" }
                val data = Arguments.createMap().apply {
                  putString("peerId", request.peer)
                  putString("event", "companion.appeared")
                }
                val task = owner.startTask(HeadlessJsTaskConfig(request.taskName, data, TASK_TIMEOUT_MS, true))
                tasks[task] = owner
                // startTask only emits synchronous start notifications; JS
                // completion is queued on this same UI thread. Register after
                // successful admission so a thrown start cannot leak a listener.
                owner.addTaskEventListener(this)
                ContinuationOutcome.Completed(ContinuationStrategy.HEADLESS_TASK, request.peer, 0, "task-dispatched")
              }
            } finally { starting.remove(request); stopIfIdle() }
          }
        } catch (error: Exception) {
          if (error is InterruptedException) Thread.currentThread().interrupt()
          request.admission.dispatch { continuationPlatformFailure(ContinuationStrategy.HEADLESS_TASK, error) }
          UiThreadUtil.runOnUiThread { starting.remove(request); stopIfIdle() }
        }
      }
    } catch (error: RuntimeException) {
      request.admission.dispatch { continuationPlatformFailure(ContinuationStrategy.HEADLESS_TASK, error) }
      starting.remove(request); stopIfIdle()
    }
    return START_NOT_STICKY
  }

  override fun onHeadlessJsTaskStart(taskId: Int) = Unit
  override fun onHeadlessJsTaskFinish(taskId: Int) {
    tasks.remove(taskId)?.let { owner -> if (!tasks.containsValue(owner)) owner.removeTaskEventListener(this) }
    stopIfIdle()
  }

  private fun stopIfIdle() {
    if (starting.isEmpty() && tasks.isEmpty()) {
      wakeLock?.let { if (it.isHeld) it.release() }
      stopSelf()
    }
  }

  override fun onDestroy() {
    destroyed = true
    starting.forEach { it.admission.cancel("operation.aborted", "Headless service destroyed before dispatch") }
    starting.clear()
    // finishTask releases RN bookkeeping, not arbitrary JavaScript execution.
    tasks.values.toSet().forEach { it.removeTaskEventListener(this) }
    tasks.forEach { (id, owner) -> if (owner.isTaskRunning(id)) owner.finishTask(id) }
    tasks.clear()
    worker.shutdown()
    wakeLock?.let { if (it.isHeld) it.release() }
    super.onDestroy()
  }

  private data class Request(val peer: String, val taskName: String, val admission: HeadlessDispatchAdmission)
  companion object {
    private const val REQUEST_KEY = "ubm.headless.request"
    private const val START_TIMEOUT_MS = 10_000L
    private const val TASK_TIMEOUT_MS = 60_000L
    private val pending = ConcurrentHashMap<String, Request>()

    /** Called only on the companion worker, never the main thread. */
    fun dispatch(context: Context, peer: String, declaration: BackgroundContinuationDeclaration): ContinuationOutcome {
      check(!UiThreadUtil.isOnUiThread()) { "Headless task startup must not wait on the main thread" }
      val name = declaration.headlessTaskName ?: return ContinuationOutcome.failed(ContinuationStrategy.HEADLESS_TASK, "argument.invalid", "Missing registered headless task name", null)
      val request = Request(peer, name, HeadlessDispatchAdmission())
      val id = UUID.randomUUID().toString()
      synchronized(pending) {
        if (pending.size >= 16) return ContinuationOutcome.failed(ContinuationStrategy.HEADLESS_TASK, "lifecycle.invalid-state", "Headless startup admission is full", null)
        pending[id] = request
      }
      try {
        val started = context.startService(Intent(context, UbmHeadlessContinuationService::class.java).putExtra(REQUEST_KEY, id))
        if (started == null) request.admission.cancel("platform.failure", "Android could not resolve the headless continuation service")
        return request.admission.await(START_TIMEOUT_MS)
      } catch (error: RuntimeException) {
        request.admission.dispatch { continuationPlatformFailure(ContinuationStrategy.HEADLESS_TASK, error) }
        return request.admission.await(0)
      } finally { pending.remove(id) }
    }
  }
}
