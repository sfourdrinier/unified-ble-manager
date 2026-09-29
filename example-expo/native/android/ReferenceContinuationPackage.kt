package com.sfourdrinier.bleplxexample.continuation

import com.facebook.react.BaseReactPackage
import com.facebook.react.bridge.NativeModule
import com.facebook.react.bridge.ReactApplicationContext
import com.facebook.react.module.model.ReactModuleInfo
import com.facebook.react.module.model.ReactModuleInfoProvider

class ReferenceContinuationPackage : BaseReactPackage() {
  override fun getModule(name: String, reactContext: ReactApplicationContext): NativeModule? =
    if (name == ReferenceContinuationModule.NAME) ReferenceContinuationModule(reactContext) else null

  override fun getReactModuleInfoProvider(): ReactModuleInfoProvider = ReactModuleInfoProvider {
    mapOf(ReferenceContinuationModule.NAME to ReactModuleInfo(
      ReferenceContinuationModule.NAME,
      ReferenceContinuationModule::class.java.name,
      false,
      false,
      false,
      false
    ))
  }
}
