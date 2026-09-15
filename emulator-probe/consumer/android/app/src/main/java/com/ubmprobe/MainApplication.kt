// emulator-probe/consumer/android/app/src/main/java/com/ubmprobe/MainApplication.kt
// Probe Application. The ReactHost is built lazily: property initializers run
// in the Application constructor before attach(), when applicationContext is
// still null, so an eager initializer would NPE on every launch.

package com.ubmprobe

import android.app.Application
import com.facebook.react.PackageList
import com.facebook.react.ReactApplication
import com.facebook.react.ReactHost
import com.facebook.react.ReactPackage
import com.facebook.react.defaults.DefaultNewArchitectureEntryPoint.load
import com.facebook.react.defaults.DefaultReactHost.getDefaultReactHost
import com.facebook.react.soloader.OpenSourceMergedSoMapping
import com.facebook.soloader.SoLoader

class MainApplication : Application(), ReactApplication {

  private val packages: List<ReactPackage> = PackageList(this).packages

  override val reactHost: ReactHost by lazy {
      getDefaultReactHost(
          applicationContext,
          packages,
          jsMainModulePath = "index",
          useDevSupport = BuildConfig.DEBUG,
      )
  }

  override fun onCreate() {
    super.onCreate()
    SoLoader.init(this, OpenSourceMergedSoMapping)
    load()
  }
}
