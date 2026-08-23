// android/src/main/java/com/sfourdrinier/unifiedblemanager/BlePlxPackage.java

package com.sfourdrinier.unifiedblemanager;

import androidx.annotation.NonNull;
import androidx.annotation.Nullable;

import com.facebook.react.BaseReactPackage;
import com.facebook.react.bridge.NativeModule;
import com.facebook.react.bridge.ReactApplicationContext;
import com.facebook.react.module.model.ReactModuleInfo;
import com.facebook.react.module.model.ReactModuleInfoProvider;
import com.sfourdrinier.unifiedblemanager.expo.UnifiedBleExpoRuntimeModule;
import com.sfourdrinier.unifiedblemanager.protocol.UnifiedBleProtocolControlModule;

import java.util.HashMap;
import java.util.Map;

public class BlePlxPackage extends BaseReactPackage {
  @Nullable
  @Override
  public NativeModule getModule(String name, ReactApplicationContext reactContext) {
    if (UnifiedBleProtocolControlModule.NAME.equals(name)) {
      return new UnifiedBleProtocolControlModule(reactContext);
    }
    if (UnifiedBleExpoRuntimeModule.NAME.equals(name)) {
      return new UnifiedBleExpoRuntimeModule(reactContext);
    }

    return null;
  }

  @NonNull
  @Override
  public ReactModuleInfoProvider getReactModuleInfoProvider() {
    return () -> {
      final Map<String, ReactModuleInfo> moduleInfos = new HashMap<>();
      moduleInfos.put(
        UnifiedBleProtocolControlModule.NAME,
        new ReactModuleInfo(
          UnifiedBleProtocolControlModule.NAME,
          UnifiedBleProtocolControlModule.class.getName(),
          false,
          false,
          false,
          true
        )
      );
      moduleInfos.put(
        UnifiedBleExpoRuntimeModule.NAME,
        new ReactModuleInfo(
          UnifiedBleExpoRuntimeModule.NAME,
          UnifiedBleExpoRuntimeModule.class.getName(),
          false,
          false,
          false,
          true
        )
      );
      return moduleInfos;
    };
  }
}
