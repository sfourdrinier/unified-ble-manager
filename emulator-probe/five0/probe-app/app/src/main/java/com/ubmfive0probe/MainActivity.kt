package com.ubmfive0probe

import android.app.Activity
import android.os.Bundle
import android.widget.TextView

/**
 * five0 probe activity: launches [Five0SelfTest] on a worker thread at
 * create (never blocks the main thread) and shows the one-line result.
 * The battery (`run-five0-battery.js`) starts this activity with `am start`
 * and matches `UBM5FIVE0-RESULT` in logcat — no UI taps, no Metro.
 */
class MainActivity : Activity() {
  override fun onCreate(savedInstanceState: Bundle?) {
    super.onCreate(savedInstanceState)
    val view = TextView(this)
    view.text = "UBM 5.0 five0 probe running…"
    setContentView(view)
    Thread({
      val json = Five0SelfTest.run(this)
      runOnUiThread { view.text = json }
    }, "five0-selftest").start()
  }
}
