#import <Foundation/Foundation.h>
#import <UIKit/UIKit.h>
#import <ReactCommon/RCTTurboModule.h>
#import <CommonCrypto/CommonDigest.h>

#if __has_include("BlePlx-Swift.h")
#import "BlePlx-Swift.h"
#endif

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

/// Finding 179: the owned-radio permission error codes
/// (OwnedCoreBluetoothProtocolRadio) as the TypeScript boundary words.
NSString *ApplePermissionErrorCode(NSInteger code) {
  switch (code) {
    case 1035: return @"permissionRestricted";
    case 1036: return @"permissionUnavailable";
    case 1037: return @"permissionInProgress";
    case 1038: return @"permissionTimeout";
    default: return @"permissionRequestFailed";
  }
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
  // Finding 179: like Android's Expo module, this presents the system
  // Bluetooth prompt and reports the decision in the Android result shape
  // ({requested, granted, denied, recommendedSettingsTarget}). The prompt is
  // the process-owned central's first allocation (same radio the Rust owner
  // drives, so there is exactly one central); a decided authorization
  // answers at once, a restriction refuses with its reason, and an
  // unanswered prompt is bounded by kApplePermissionTimeoutMs. The caller
  // races its own timeout/signal in TypeScript; a late native answer is
  // discarded there.
  static const double kApplePermissionTimeoutMs = 300000;
  if (![request.purpose() isEqualToString:@"scan-and-connect"]) {
    reject(@"permissionInvalidPurpose",
           @"The Expo permission purpose must be scan-and-connect.",
           nil);
    return;
  }
#if __has_include("BlePlx-Swift.h")
  OwnedCoreBluetoothProtocolRadio *radio = [UnifiedBleRustCoreSessions radioForPermissionPrompt];
  [radio requestPermission:[NSNumber numberWithDouble:kApplePermissionTimeoutMs]
                completion:^(NSDictionary *result, NSError *error) {
                  if (error != nil) {
                    reject(ApplePermissionErrorCode(error.code),
                           error.localizedDescription,
                           nil);
                    return;
                  }
                  resolve(result);
                }];
#else
  reject(@"permissionUnavailable",
         @"The Apple permission prompt needs the process radio, which is unavailable in this build.",
         nil);
#endif
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
