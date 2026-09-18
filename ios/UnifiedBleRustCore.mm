// ios/UnifiedBleRustCore.mm
//
// ObjC++ shell of the `UnifiedBleRustCore` TurboModule
// (src/NativeUnifiedBleRustCore.ts, frozen contract in docs/MOBILE_RUST_WIRE.md).
// It maps promises onto `UnifiedBleRustCoreSessions` and nothing else: Rust
// writes every resolved JSON string, and every rejection message is the wire
// failure JSON `{"code","domain","operation","detail"}`. New Architecture
// only: the class is the codegen `NativeUnifiedBleRustCoreSpecBase` subclass
// so `onSessionWake` reaches JavaScript through the generated emitter.

#import <Foundation/Foundation.h>
#import <CoreBluetooth/CoreBluetooth.h>
#import <React/RCTBridgeModule.h>
#import <React/RCTInvalidating.h>
#import <ReactCommon/RCTTurboModule.h>

#if __has_include("BlePlx-Swift.h")
#import "BlePlx-Swift.h"
#endif

#ifdef RCT_NEW_ARCH_ENABLED
#import <UnifiedBleProtocolSpec/UnifiedBleProtocolSpec.h>

namespace {

void rejectWithFailure(RCTPromiseRejectBlock reject, NSString *failureJson) {
  reject(@"UnifiedBleRustCore", failureJson, nil);
}

} // namespace

@interface UnifiedBleRustCore : NativeUnifiedBleRustCoreSpecBase <NativeUnifiedBleRustCoreSpec, RCTInvalidating>
@end

@implementation UnifiedBleRustCore

RCT_EXPORT_MODULE(UnifiedBleRustCore)

- (instancetype)init {
  self = [super init];
  if (self != nil) {
    // Restoration: CoreBluetooth hands restored peripherals only to a central
    // created with the restore identifier, so the process host and its radio
    // come up when JavaScript first loads this module (legacy timing). A
    // failure here is reported again, as data, by every openSession.
    NSString *failure = [[UnifiedBleRustCoreSessions shared] ensureHost];
    if (failure != nil) {
      NSLog(@"[UnifiedBleRustCore] process host install failed: %@", failure);
    }
  }
  return self;
}

- (std::shared_ptr<facebook::react::TurboModule>)getTurboModule:
    (const facebook::react::ObjCTurboModule::InitParams &)params {
  return std::make_shared<facebook::react::NativeUnifiedBleRustCoreSpecJSI>(params);
}

- (void)invalidate {
  [[UnifiedBleRustCoreSessions shared] closeSessionsOwnedBy:self];
}

- (void)openSession:(NSString *)owner
    expectedWireRevision:(NSString *)expectedWireRevision
                 resolve:(RCTPromiseResolveBlock)resolve
                  reject:(RCTPromiseRejectBlock)reject {
  __weak UnifiedBleRustCore *weakSelf = self;
  [[UnifiedBleRustCoreSessions shared] openSession:owner
      expectedWireRevision:expectedWireRevision
      ownerToken:self
      onWake:^(NSString *sessionId) {
        [weakSelf emitOnSessionWake:@{@"sessionId": sessionId}];
      }
      completion:^(NSString *admission, NSString *failure) {
        if (failure != nil) {
          rejectWithFailure(reject, failure);
          return;
        }
        resolve(admission);
      }];
}

- (void)invoke:(NSString *)sessionId
            op:(NSString *)op
      argsJson:(NSString *)argsJson
       resolve:(RCTPromiseResolveBlock)resolve
        reject:(RCTPromiseRejectBlock)reject {
  [[UnifiedBleRustCoreSessions shared] invokeWithSessionId:sessionId
                                                        op:op
                                                  argsJson:argsJson
                                                completion:^(NSString *envelope, NSString *failure) {
                                                  if (failure != nil) {
                                                    rejectWithFailure(reject, failure);
                                                    return;
                                                  }
                                                  resolve(envelope);
                                                }];
}

- (void)drain:(NSString *)sessionId
     maxItems:(double)maxItems
     maxBytes:(double)maxBytes
      resolve:(RCTPromiseResolveBlock)resolve
       reject:(RCTPromiseRejectBlock)reject {
  [[UnifiedBleRustCoreSessions shared] drainWithSessionId:sessionId
                                                 maxItems:maxItems
                                                 maxBytes:maxBytes
                                               completion:^(NSString *records, NSString *failure) {
                                                 if (failure != nil) {
                                                   rejectWithFailure(reject, failure);
                                                   return;
                                                 }
                                                 resolve(records);
                                               }];
}

- (void)closeSession:(NSString *)sessionId
             resolve:(RCTPromiseResolveBlock)resolve
              reject:(RCTPromiseRejectBlock)reject {
  [[UnifiedBleRustCoreSessions shared] closeSession:sessionId
                                         completion:^(NSString *failure) {
                                           if (failure != nil) {
                                             rejectWithFailure(reject, failure);
                                             return;
                                           }
                                           resolve(nil);
                                         }];
}

- (void)nativeBuildIdentity:(RCTPromiseResolveBlock)resolve reject:(RCTPromiseRejectBlock)reject {
  resolve([[UnifiedBleRustCoreSessions shared] nativeBuildIdentity]);
}

- (void)contractRevision:(RCTPromiseResolveBlock)resolve reject:(RCTPromiseRejectBlock)reject {
  resolve([[UnifiedBleRustCoreSessions shared] contractRevision]);
}

- (void)wireRevision:(RCTPromiseResolveBlock)resolve reject:(RCTPromiseRejectBlock)reject {
  resolve([[UnifiedBleRustCoreSessions shared] wireRevision]);
}

- (void)randomBytes:(double)length resolve:(RCTPromiseResolveBlock)resolve reject:(RCTPromiseRejectBlock)reject {
  [[UnifiedBleRustCoreSessions shared] randomBytes:length
                                        completion:^(NSString *base64, NSString *failure) {
                                          if (failure != nil) {
                                            rejectWithFailure(reject, failure);
                                            return;
                                          }
                                          resolve(base64);
                                        }];
}

- (void)restorationIdentity:(NSString *)requestJson
                    resolve:(RCTPromiseResolveBlock)resolve
                     reject:(RCTPromiseRejectBlock)reject {
  [[UnifiedBleRustCoreSessions shared] restorationIdentity:requestJson
                                                completion:^(NSString *identity, NSString *failure) {
                                                  if (failure != nil) {
                                                    rejectWithFailure(reject, failure);
                                                    return;
                                                  }
                                                  resolve(identity);
                                                }];
}

@end

#endif
