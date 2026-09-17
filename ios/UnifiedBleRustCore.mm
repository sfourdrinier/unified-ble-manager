// ios/UnifiedBleRustCore.mm
//
// R01/D3(a) ObjC++ shell for the `UnifiedBleRustCore` TurboModule. Thin
// bridge only: session ownership and the op table live in
// `UnifiedBleRustCoreSessions.swift` (mirroring Android's
// `RustCoreSessionRouter`); this file maps results onto RN promises.

#import <Foundation/Foundation.h>
#import <React/RCTLog.h>

#if __has_include("BlePlx-Swift.h")
#import "BlePlx-Swift.h"
#endif

@interface UnifiedBleRustCore : NSObject <RCTBridgeModule>
@end

@implementation UnifiedBleRustCore

RCT_EXPORT_MODULE(UnifiedBleRustCore)

RCT_EXPORT_METHOD(openSession:(NSString *)owner
                  resolver:(RCTPromiseResolveBlock)resolve
                  rejecter:(RCTPromiseRejectBlock)reject) {
  NSError *error = nil;
  NSString *sessionId = [[UnifiedBleRustCoreSessions shared] openSession:owner error:&error];
  if (error != nil) {
    reject(error.domain, error.localizedDescription, error);
    return;
  }
  resolve(@{@"sessionId": sessionId});
}

RCT_EXPORT_METHOD(invoke:(NSString *)sessionId
                  op:(NSString *)op
                  argsJson:(NSString *)argsJson
                  resolver:(RCTPromiseResolveBlock)resolve
                  rejecter:(RCTPromiseRejectBlock)reject) {
  // Domain failures travel as data (the frozen wire record), never as
  // rejections — the Swift layer only returns records.
  NSDictionary *record = [[UnifiedBleRustCoreSessions shared] invokeWithSessionId:sessionId
                                                                               op:op
                                                                         argsJson:argsJson];
  resolve(record);
}

RCT_EXPORT_METHOD(close:(NSString *)sessionId
                  resolver:(RCTPromiseResolveBlock)resolve
                  rejecter:(RCTPromiseRejectBlock)reject) {
  [[UnifiedBleRustCoreSessions shared] closeSession:sessionId];
  resolve(nil);
}

RCT_EXPORT_METHOD(contractRevision:(RCTPromiseResolveBlock)resolve
                  rejecter:(RCTPromiseRejectBlock)reject) {
  resolve([[UnifiedBleRustCoreSessions shared] revision]);
}

@end
