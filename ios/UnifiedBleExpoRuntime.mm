#import <Foundation/Foundation.h>
#import <UIKit/UIKit.h>
#import <ReactCommon/RCTTurboModule.h>
#import <CommonCrypto/CommonDigest.h>

#ifdef RCT_NEW_ARCH_ENABLED
#import <UnifiedBleProtocolSpec/UnifiedBleProtocolSpec.h>
#endif

namespace {

bool validString(NSString *value) {
  return value != nil && value.length > 0;
}

NSString *configuredInfoString(NSString *key) {
  id value = [[NSBundle mainBundle] objectForInfoDictionaryKey:key];
  return [value isKindOfClass:[NSString class]] && validString(value) ? value : @"";
}

NSString *configuredBackgroundModes(void) {
  id value = [[NSBundle mainBundle] objectForInfoDictionaryKey:@"UIBackgroundModes"];
  if (![value isKindOfClass:[NSArray class]]) return @"";
  NSArray *modes = [(NSArray *)value filteredArrayUsingPredicate:
      [NSPredicate predicateWithBlock:^BOOL(id object, NSDictionary *bindings) {
        return [object isKindOfClass:[NSString class]];
      }]];
  modes = [modes sortedArrayUsingSelector:@selector(compare:)];
  return [modes componentsJoinedByString:@","];
}

NSString *sha256Hex(NSString *value) {
  NSData *data = [value dataUsingEncoding:NSUTF8StringEncoding];
  unsigned char digest[CC_SHA256_DIGEST_LENGTH];
  CC_SHA256(data.bytes, static_cast<CC_LONG>(data.length), digest);
  NSMutableString *result = [NSMutableString stringWithCapacity:CC_SHA256_DIGEST_LENGTH * 2];
  for (NSUInteger index = 0; index < CC_SHA256_DIGEST_LENGTH; index++) {
    [result appendFormat:@"%02x", digest[index]];
  }
  return result;
}

NSString *configurationDigest(void) {
  NSString *canonical = [NSString stringWithFormat:
      @"unified-ble-expo-runtime-v1\n"
       "platform=apple\n"
       "bluetoothUsageDescription=%@\n"
       "backgroundModes=%@\n"
       "restorationId=%@\n"
       "restorationGeneration=%@\n"
       "showPowerAlert=%@\n"
       "nativeLogging=%@\n",
      configuredInfoString(@"NSBluetoothAlwaysUsageDescription"),
      configuredBackgroundModes(),
      configuredInfoString(@"UnifiedBleProtocolRestorationId"),
      configuredInfoString(@"UnifiedBleProtocolRestorationGeneration"),
      [[NSBundle mainBundle] objectForInfoDictionaryKey:@"UnifiedBleProtocolShowPowerAlert"] ?: @"",
      configuredInfoString(@"UnifiedBleProtocolNativeLogging")];
  return sha256Hex(canonical);
}

} // namespace

#ifdef RCT_NEW_ARCH_ENABLED

@interface UnifiedBleExpoRuntime : NSObject <NativeUnifiedBleExpoRuntimeSpec>
@end

@implementation UnifiedBleExpoRuntime

RCT_EXPORT_MODULE(UnifiedBleExpoRuntime)

- (std::shared_ptr<facebook::react::TurboModule>)getTurboModule:
    (const facebook::react::ObjCTurboModule::InitParams &)params {
  return std::make_shared<facebook::react::NativeUnifiedBleExpoRuntimeSpecJSI>(params);
}

- (void)getRuntimeConfiguration:(RCTPromiseResolveBlock)resolve
                         reject:(RCTPromiseRejectBlock)reject {
  if (![configuredInfoString(@"UnifiedBlePluginConfigurationMarker") isEqualToString:@"unified-ble-expo-v1"]) {
    reject(@"nativeConfigurationMissing",
           @"The Unified BLE Expo plugin configuration marker is absent; run expo prebuild and rebuild the native app.",
           nil);
    return;
  }
  resolve(@{
    @"platform": @"apple",
    @"configurationDigest": configurationDigest(),
  });
}

- (void)requestPermissions:(JS::NativeUnifiedBleExpoRuntime::NativeExpoPermissionRequest &)request
                   resolve:(RCTPromiseResolveBlock)resolve
                    reject:(RCTPromiseRejectBlock)reject {
  (void)request;
  (void)resolve;
  reject(@"unsupportedPermissionPrompt",
         @"iOS has no standalone Bluetooth permission prompt; invoke a Bluetooth action first, then re-read readiness.",
         nil);
}

- (void)openSettings:(JS::NativeUnifiedBleExpoRuntime::NativeExpoSettingsRequest &)request
             resolve:(RCTPromiseResolveBlock)resolve
              reject:(RCTPromiseRejectBlock)reject {
  NSString *target = request.target();
  if (![target isEqualToString:@"app"]) {
    reject(@"settingsUnsupported",
           @"iOS exposes only the application settings URL through this Expo bridge; Bluetooth and location settings cannot be targeted reliably.",
           nil);
    return;
  }
  dispatch_async(dispatch_get_main_queue(), ^{
    NSURL *url = [NSURL URLWithString:UIApplicationOpenSettingsURLString];
    UIApplication *application = UIApplication.sharedApplication;
    if (url == nil || ![application canOpenURL:url]) {
      reject(@"settingsUnavailable", @"The iOS application settings URL is unavailable.", nil);
      return;
    }
    [application openURL:url options:@{} completionHandler:^(BOOL success) {
      if (success) resolve(nil);
      else reject(@"settingsUnavailable", @"The iOS application settings screen could not be opened.", nil);
    }];
  });
}

@end

#endif
