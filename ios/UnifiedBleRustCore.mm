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
#import <React/RCTLog.h>
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

@interface UnifiedBleRustCore : NativeUnifiedBleRustCoreSpecBase <NativeUnifiedBleRustCoreSpec, RCTInvalidating> {
  NSMutableSet<NSString *> *_accessoryChoiceRequests;
  BOOL _accessoryChoicesInvalidated;
}
@end

@implementation UnifiedBleRustCore

RCT_EXPORT_MODULE(UnifiedBleRustCore)

// Loading this module also exposes offline recording controls. Radio ownership
// begins at openSession or the separate configured native launch bootstrap,
// never as a side effect of requesting retained data.

- (std::shared_ptr<facebook::react::TurboModule>)getTurboModule:
    (const facebook::react::ObjCTurboModule::InitParams &)params {
  return std::make_shared<facebook::react::NativeUnifiedBleRustCoreSpecJSI>(params);
}

- (void)invalidate {
  [[UnifiedBleRustCoreSessions shared] closeSessionsOwnedBy:self];
  dispatch_async(dispatch_get_main_queue(), ^{
    self->_accessoryChoicesInvalidated = YES;
    for (NSString *requestId in [self->_accessoryChoiceRequests copy]) {
      [[UnifiedBleAccessoryChooser shared] cancel:requestId completion:^(NSString *failure) {
        if (failure != nil) RCTLogError(@"UBM accessory cleanup refused: %@", failure);
      }];
    }
  });
}

- (void)chooseAccessory:(NSString *)requestId optionsJson:(NSString *)optionsJson timeoutMs:(double)timeoutMs
                 resolve:(RCTPromiseResolveBlock)resolve reject:(RCTPromiseRejectBlock)reject {
  dispatch_async(dispatch_get_main_queue(), ^{
    if (self->_accessoryChoicesInvalidated) {
      rejectWithFailure(reject, @"{\"code\":\"chooser.closed\",\"domain\":\"chooser\",\"operation\":\"accessory.choose\",\"detail\":\"module invalidated\"}");
      return;
    }
    if (self->_accessoryChoiceRequests == nil) self->_accessoryChoiceRequests = [NSMutableSet new];
    [self->_accessoryChoiceRequests addObject:requestId];
    [[UnifiedBleAccessoryChooser shared] choose:requestId optionsJson:optionsJson timeoutMs:timeoutMs
      completion:^(NSString *result, NSString *failure) {
        [self->_accessoryChoiceRequests removeObject:requestId];
        if (failure != nil) rejectWithFailure(reject, failure); else resolve(result);
      }];
  });
}

- (void)cancelAccessoryChoice:(NSString *)requestId resolve:(RCTPromiseResolveBlock)resolve reject:(RCTPromiseRejectBlock)reject {
  [[UnifiedBleAccessoryChooser shared] cancel:requestId completion:^(NSString *failure) {
    if (failure != nil) rejectWithFailure(reject, failure); else resolve(nil);
  }];
}

- (void)accessoryChooserAvailable:(RCTPromiseResolveBlock)resolve reject:(RCTPromiseRejectBlock)reject {
  resolve(@([[UnifiedBleAccessoryChooser shared] available]));
}

- (void)authorizedAccessories:(RCTPromiseResolveBlock)resolve reject:(RCTPromiseRejectBlock)reject {
  dispatch_async(dispatch_get_main_queue(), ^{
    if (self->_accessoryChoicesInvalidated) {
      rejectWithFailure(reject, @"{\"code\":\"chooser.closed\",\"domain\":\"chooser\",\"operation\":\"accessory.authorized\",\"detail\":\"module invalidated\"}");
      return;
    }
    [[UnifiedBleRustCoreSessions shared] authorizedAccessories:^(NSString *result, NSString *failure) {
      if (self->_accessoryChoicesInvalidated) {
        rejectWithFailure(reject, @"{\"code\":\"chooser.closed\",\"domain\":\"chooser\",\"operation\":\"accessory.authorized\",\"detail\":\"module invalidated\"}");
      } else if (failure != nil) rejectWithFailure(reject, failure);
      else resolve(result);
    }];
  });
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

- (void)declareBackgroundContinuation:(NSString *)declarationJson
                              resolve:(RCTPromiseResolveBlock)resolve
                               reject:(RCTPromiseRejectBlock)reject {
  [[UnifiedBleRustCoreSessions shared] declareBackgroundContinuation:declarationJson
                                                          completion:^(NSString *state, NSString *failure) {
                                                            if (failure != nil) {
                                                              rejectWithFailure(reject, failure);
                                                              return;
                                                            }
                                                            resolve(state);
                                                          }];
}

- (void)recordingControl:(NSString *)operation id:(NSString *)recordingId token:(NSString *)token maxItems:(double)maxItems maxBytes:(double)maxBytes resolve:(RCTPromiseResolveBlock)resolve reject:(RCTPromiseRejectBlock)reject {
  [[UnifiedBleRustCoreSessions shared] recordingControl:operation id:recordingId token:token maxItems:maxItems maxBytes:maxBytes completion:^(NSString *value, NSString *failure) {
    if (failure != nil) { rejectWithFailure(reject, failure); return; }
    resolve(value);
  }];
}
- (void)continuationRecordingStatus:(NSString *)recordingId resolve:(RCTPromiseResolveBlock)resolve reject:(RCTPromiseRejectBlock)reject {
  [self recordingControl:@"status" id:recordingId token:@"" maxItems:0 maxBytes:0 resolve:resolve reject:reject];
}
- (void)continuationRecordingPrepare:(NSString *)recordingId maxItems:(double)maxItems maxBytes:(double)maxBytes resolve:(RCTPromiseResolveBlock)resolve reject:(RCTPromiseRejectBlock)reject {
  [self recordingControl:@"prepare" id:recordingId token:@"" maxItems:maxItems maxBytes:maxBytes resolve:resolve reject:reject];
}
- (void)continuationRecordingAcknowledge:(NSString *)recordingId token:(NSString *)token resolve:(RCTPromiseResolveBlock)resolve reject:(RCTPromiseRejectBlock)reject {
  [self recordingControl:@"acknowledge" id:recordingId token:token maxItems:0 maxBytes:0 resolve:resolve reject:reject];
}
- (void)continuationRecordingStop:(NSString *)recordingId resolve:(RCTPromiseResolveBlock)resolve reject:(RCTPromiseRejectBlock)reject {
  [self recordingControl:@"stop" id:recordingId token:@"" maxItems:0 maxBytes:0 resolve:resolve reject:reject];
}
- (void)continuationRecordingClear:(NSString *)recordingId resolve:(RCTPromiseResolveBlock)resolve reject:(RCTPromiseRejectBlock)reject {
  [self recordingControl:@"clear" id:recordingId token:@"" maxItems:0 maxBytes:0 resolve:resolve reject:reject];
}
- (void)continuationStatus:(RCTPromiseResolveBlock)resolve reject:(RCTPromiseRejectBlock)reject {
  [[UnifiedBleRustCoreSessions shared] continuationStatus:^(NSString *status, NSString *failure) {
    if (failure != nil) {
      rejectWithFailure(reject, failure);
      return;
    }
    resolve(status);
  }];
}

- (void)prepareContinuationClaim:(double)maxItems
                maxBytes:(double)maxBytes
                 resolve:(RCTPromiseResolveBlock)resolve
                  reject:(RCTPromiseRejectBlock)reject {
  [[UnifiedBleRustCoreSessions shared] prepareContinuationClaimWithMaxItems:maxItems
                                                           maxBytes:maxBytes
                                                         completion:^(NSString *claim, NSString *failure) {
                                                           if (failure != nil) {
                                                             rejectWithFailure(reject, failure);
                                                             return;
                                                           }
                                                           resolve(claim);
                                                         }];
}

- (void)acknowledgeContinuationClaim:(NSString *)claimToken
                 resolve:(RCTPromiseResolveBlock)resolve
                  reject:(RCTPromiseRejectBlock)reject {
  [[UnifiedBleRustCoreSessions shared] acknowledgeContinuationClaim:claimToken
                                                         completion:^(NSString *claim, NSString *failure) {
                                                           if (failure != nil) {
                                                             rejectWithFailure(reject, failure);
                                                             return;
                                                           }
                                                           resolve(claim);
                                                         }];
}

@end

#endif
