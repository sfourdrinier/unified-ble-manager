// ios/UnifiedBleRustCore.mm
//
// R01/D3(a) ObjC++ shell for the `UnifiedBleRustCore` TurboModule. Thin
// bridge only: session ownership and the op table live in
// `UnifiedBleRustCoreSessions.swift` (mirroring Android's
// `RustCoreSessionRouter`); this file maps results onto RN promises.
// New Arch only, like the sibling protocol control module: the class
// conforms to the codegen `NativeUnifiedBleRustCoreSpec` (promise methods
// use the spec `resolve:`/`reject:` selectors, never the old-arch
// `RCT_EXPORT_METHOD` bridge macros).

#import <Foundation/Foundation.h>
#import <CoreBluetooth/CoreBluetooth.h>
#import <React/RCTBridgeModule.h>
#import <React/RCTLog.h>
#import <ReactCommon/RCTTurboModule.h>

#if __has_include("BlePlx-Swift.h")
#import "BlePlx-Swift.h"
#endif

#ifdef RCT_NEW_ARCH_ENABLED
#import <UnifiedBleProtocolSpec/UnifiedBleProtocolSpec.h>

@interface UnifiedBleRustCore : NSObject <NativeUnifiedBleRustCoreSpec>
@end

@implementation UnifiedBleRustCore

RCT_EXPORT_MODULE(UnifiedBleRustCore)

- (std::shared_ptr<facebook::react::TurboModule>)getTurboModule:
    (const facebook::react::ObjCTurboModule::InitParams &)params {
  return std::make_shared<facebook::react::NativeUnifiedBleRustCoreSpecJSI>(params);
}

- (void)openSession:(NSString *)owner
            resolve:(RCTPromiseResolveBlock)resolve
             reject:(RCTPromiseRejectBlock)reject {
  NSError *error = nil;
  NSString *sessionId = [[UnifiedBleRustCoreSessions shared] openSession:owner error:&error];
  if (error != nil) {
    reject(error.domain, error.localizedDescription, error);
    return;
  }
  resolve(@{@"sessionId": sessionId});
}

- (void)invoke:(NSString *)sessionId
            op:(NSString *)op
      argsJson:(NSString *)argsJson
       resolve:(RCTPromiseResolveBlock)resolve
        reject:(RCTPromiseRejectBlock)reject {
  // Domain failures travel as data (the frozen wire record), never as
  // rejections — the Swift layer only returns records.
  NSDictionary *record = [[UnifiedBleRustCoreSessions shared] invokeWithSessionId:sessionId
                                                                               op:op
                                                                         argsJson:argsJson];
  resolve(record);
}

- (void)close:(NSString *)sessionId
      resolve:(RCTPromiseResolveBlock)resolve
       reject:(RCTPromiseRejectBlock)reject {
  [[UnifiedBleRustCoreSessions shared] closeSession:sessionId];
  resolve(nil);
}

- (void)contractRevision:(RCTPromiseResolveBlock)resolve
                  reject:(RCTPromiseRejectBlock)reject {
  resolve([[UnifiedBleRustCoreSessions shared] revision]);
}

@end

#endif
