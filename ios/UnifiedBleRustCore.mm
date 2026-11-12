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
#import <ReactCommon/RCTTurboModuleWithJSIBindings.h>

#include <jsi/jsi.h>
#include <atomic>
#include <cmath>
#include <cstring>

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

namespace {
using namespace facebook;

class ScanFailureRecoveryRuntime final : public jsi::HostObject {
 public:
  explicit ScanFailureRecoveryRuntime(std::weak_ptr<std::atomic_bool> live) : live_(std::move(live)) {}

  jsi::Value get(jsi::Runtime &runtime, const jsi::PropNameID &name) override {
    const auto property = name.utf8(runtime);
    if (property == "rawToken") return rawTokenFunction(runtime, name);
    if (property != "read") return jsi::Value::undefined();
    return jsi::Function::createFromHostFunction(runtime, name, 2U,
      [live = live_](jsi::Runtime &inner, const jsi::Value &, const jsi::Value *args, size_t count) {
        const auto held = live.lock();
        if (!held || !held->load(std::memory_order_acquire))
          throw jsi::JSError(inner, "scan failure recovery runtime is closed");
        if (count != 2U || !args[0].isString() || !args[1].isString())
          throw jsi::JSError(inner, "scan failure recovery read requires opaque head and offset strings");
        @try {
          NSError *error = nil;
          const auto head = args[0].asString(inner).utf8(inner);
          const auto offset = args[1].asString(inner).utf8(inner);
          auto *page = [[UnifiedBleRustCoreSessions shared]
              readReactNativeScanFailureRecoveryPage:[NSString stringWithUTF8String:head.c_str()]
              offset:[NSString stringWithUTF8String:offset.c_str()]
              error:&error];
          if (page == nil || error != nil)
            throw jsi::JSError(inner, error == nil ? "scan failure recovery read failed" : error.localizedDescription.UTF8String);
          const auto length = page.bytes.length;
          if (length > 64U * 1024U) throw jsi::JSError(inner, "scan failure recovery page exceeds 64 KiB");
          jsi::Object result(inner);
          result.setProperty(inner, "offset", jsi::String::createFromUtf8(inner, page.offset.UTF8String));
          result.setProperty(inner, "nextOffset", jsi::String::createFromUtf8(inner, page.nextOffset.UTF8String));
          result.setProperty(inner, "done", page.done);
          jsi::Uint8Array bytes(inner, length);
          auto buffer = bytes.buffer(inner);
          if (length != 0U && buffer.data(inner) == nullptr)
            throw jsi::JSError(inner, "scan failure recovery bytes allocation failed");
          if (length != 0U) std::memcpy(buffer.data(inner), page.bytes.bytes, length);
          result.setProperty(inner, "bytes", bytes);
          if (!held->load(std::memory_order_acquire))
            throw jsi::JSError(inner, "scan failure recovery runtime closed during read");
          return jsi::Value(inner, result);
        } @catch (NSException *exception) {
          throw jsi::JSError(inner, exception.reason.UTF8String);
        }
      });
  }

  jsi::Value rawTokenFunction(jsi::Runtime &runtime, const jsi::PropNameID &name) {
    return jsi::Function::createFromHostFunction(runtime, name, 4U,
      [live = live_](jsi::Runtime &inner, const jsi::Value &, const jsi::Value *args, size_t count) {
        const auto held = live.lock();
        if (!held || !held->load(std::memory_order_acquire)) throw jsi::JSError(inner, "scan failure recovery runtime is closed");
        if (count != 4U || !args[0].isString() || !args[1].isNumber() || !args[2].isString() || !args[3].isString()) throw jsi::JSError(inner, "rawToken requires owner, index, offset, and canonical maxBytes strings");
        const auto index = args[1].asNumber();
        if (!std::isfinite(index) || index < 0 || index > 9007199254740991.0 || std::floor(index) != index) throw jsi::JSError(inner, "rawToken index is invalid");
        @try {
          NSError *error = nil;
          const auto owner = args[0].asString(inner).utf8(inner); const auto offset = args[2].asString(inner).utf8(inner); const auto maxBytes = args[3].asString(inner).utf8(inner);
          auto *token = [[UnifiedBleRustCoreSessions shared] readReactNativeScanFailureRecoveryRawToken:[NSString stringWithUTF8String:owner.c_str()] tokenIndex:index offset:[NSString stringWithUTF8String:offset.c_str()] maxBytes:[NSString stringWithUTF8String:maxBytes.c_str()] error:&error];
          if (token == nil || error != nil) throw jsi::JSError(inner, error == nil ? "rawToken failed" : error.localizedDescription.UTF8String);
          jsi::Object result(inner); result.setProperty(inner, "kind", jsi::String::createFromUtf8(inner, token.kind.UTF8String));
          if (token.value != nil) {
            if ([token.value isKindOfClass:[NSNumber class]]) result.setProperty(inner, "value", [(NSNumber *)token.value boolValue]);
            else result.setProperty(inner, "value", jsi::String::createFromUtf8(inner, [(NSString *)token.value UTF8String]));
          }
          if (token.count != nil) result.setProperty(inner, "count", jsi::String::createFromUtf8(inner, token.count.UTF8String));
          if (token.total != nil) result.setProperty(inner, "total", jsi::String::createFromUtf8(inner, token.total.UTF8String));
          if (token.bytes != nil) { const auto length = token.bytes.length; if (length > 65536U) throw jsi::JSError(inner, "rawToken bytes exceed 64 KiB"); jsi::Uint8Array bytes(inner, length); auto buffer = bytes.buffer(inner); if (length != 0U && buffer.data(inner) == nullptr) throw jsi::JSError(inner, "rawToken bytes allocation failed"); if (length != 0U) std::memcpy(buffer.data(inner), token.bytes.bytes, length); result.setProperty(inner, "bytes", bytes); }
          if (!held->load(std::memory_order_acquire)) throw jsi::JSError(inner, "scan failure recovery runtime closed during rawToken");
          return jsi::Value(inner, result);
        } @catch (NSException *exception) { throw jsi::JSError(inner, exception.reason.UTF8String); }
      });
  }

 private:
  std::weak_ptr<std::atomic_bool> live_;
};
} // namespace

@interface UnifiedBleRustCore : NativeUnifiedBleRustCoreSpecBase <NativeUnifiedBleRustCoreSpec, RCTInvalidating, RCTTurboModuleWithJSIBindings> {
  NSMutableSet<NSString *> *_accessoryChoiceRequests;
  BOOL _accessoryChoicesInvalidated;
  std::shared_ptr<std::atomic_bool> _scanFailureRecoveryJsiLive;
}
@end

@implementation UnifiedBleRustCore

- (void)configureTerminalEvidenceMaximumBytes:(NSString *)maximumBytes
                                      resolve:(RCTPromiseResolveBlock)resolve
                                       reject:(RCTPromiseRejectBlock)reject {
  @try { [[UnifiedBleRustCoreSessions shared] configureTerminalEvidenceMaximumBytes:maximumBytes]; resolve(nil); }
  @catch (NSException *error) { reject(@"configureTerminalEvidenceMaximumBytes", error.reason, error); }
}

RCT_EXPORT_MODULE(UnifiedBleRustCore)

- (instancetype)init {
  self = [super init];
  if (self != nil) _scanFailureRecoveryJsiLive = std::make_shared<std::atomic_bool>(true);
  return self;
}

- (void)installJSIBindingsWithRuntime:(facebook::jsi::Runtime &)runtime
                          callInvoker:(const std::shared_ptr<facebook::react::CallInvoker> &)callInvoker {
  (void)callInvoker;
  if (!_scanFailureRecoveryJsiLive || !_scanFailureRecoveryJsiLive->load(std::memory_order_acquire)) return;
  // Rebind on every module installation. A React Native module can be
  // invalidated and recreated against the same JSI runtime; retaining the old
  // host object would retain its deliberately-closed lifetime fence forever.
  // The host remains capability-free: its operations are selection and
  // binding fenced in Swift and expose neither recording paths nor ids.
  static constexpr const char *kName = "__unifiedBleRustCoreScanFailureRecoveryV1";
  runtime.global().setProperty(runtime, kName,
    facebook::jsi::Object::createFromHostObject(runtime, std::make_shared<ScanFailureRecoveryRuntime>(_scanFailureRecoveryJsiLive)));
}

// Loading this module also exposes offline recording controls. Radio ownership
// begins at openSession or the separate configured native launch bootstrap,
// never as a side effect of requesting retained data.

- (std::shared_ptr<facebook::react::TurboModule>)getTurboModule:
    (const facebook::react::ObjCTurboModule::InitParams &)params {
  auto module = std::make_shared<facebook::react::NativeUnifiedBleRustCoreSpecJSI>(params);
  // SpecJSI installs the emitter callback during construction. Observe only
  // afterward so cleanup on another native thread cannot emit during setup.
  __weak UnifiedBleRustCore *weakSelf = self;
  [[UnifiedBleRustCoreSessions shared] addRecoveryObserver:self callback:^{
    [weakSelf emitOnRecoveryHint:@{@"kind": @"cleanup"}];
  }];
  return module;
}

- (void)invalidate {
  if (_scanFailureRecoveryJsiLive) _scanFailureRecoveryJsiLive->store(false, std::memory_order_release);
  [[UnifiedBleRustCoreSessions shared] removeRecoveryObserver:self];
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

- (void)negotiateScanEndVersion:(NSString *)sessionId
                    offeredJson:(NSString *)offeredJson
                        resolve:(RCTPromiseResolveBlock)resolve
                         reject:(RCTPromiseRejectBlock)reject {
  [[UnifiedBleRustCoreSessions shared] negotiateScanEndVersionWithSessionId:sessionId
      offeredJson:offeredJson ownerToken:self completion:^(NSString *value, NSString *failure) {
        if (failure != nil) rejectWithFailure(reject, failure); else resolve(value);
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

- (void)orderedRecordingVersion:(RCTPromiseResolveBlock)resolve reject:(RCTPromiseRejectBlock)reject {
  resolve(@([[UnifiedBleRustCoreSessions shared] orderedRecordingVersion]));
}
- (void)orderedRecordingReceipt:(NSString *)operation sessionId:(NSString *)sessionId id:(NSString *)recordingId resolve:(RCTPromiseResolveBlock)resolve reject:(RCTPromiseRejectBlock)reject {
  [[UnifiedBleRustCoreSessions shared] orderedRecordingReceiptControlWithSessionId:sessionId id:recordingId operation:operation ownerToken:self completion:^(NSString *value, NSString *failure) {
    if (failure != nil) { rejectWithFailure(reject, failure); return; } resolve(value);
  }];
}
- (void)orderedRecordingReserve:(NSString *)sessionId id:(NSString *)recordingId resolve:(RCTPromiseResolveBlock)resolve reject:(RCTPromiseRejectBlock)reject {
  [self orderedRecordingReceipt:@"reserve" sessionId:sessionId id:recordingId resolve:resolve reject:reject];
}
- (void)orderedRecordingStartStatus:(NSString *)sessionId id:(NSString *)recordingId resolve:(RCTPromiseResolveBlock)resolve reject:(RCTPromiseRejectBlock)reject {
  [self orderedRecordingReceipt:@"status" sessionId:sessionId id:recordingId resolve:resolve reject:reject];
}
- (void)orderedRecordingRetire:(NSString *)sessionId id:(NSString *)recordingId resolve:(RCTPromiseResolveBlock)resolve reject:(RCTPromiseRejectBlock)reject {
  [self orderedRecordingReceipt:@"retire" sessionId:sessionId id:recordingId resolve:resolve reject:reject];
}
- (void)orderedRecordingStart:(NSString *)sessionId handle:(NSString *)handle id:(NSString *)recordingId maxBytes:(NSString *)maxBytes maxRecords:(NSString *)maxRecords declarationJson:(NSString *)declarationJson budgetMs:(double)budgetMs resolve:(RCTPromiseResolveBlock)resolve reject:(RCTPromiseRejectBlock)reject {
  [[UnifiedBleRustCoreSessions shared] orderedRecordingStartWithSessionId:sessionId handle:handle id:recordingId maxBytes:maxBytes maxRecords:maxRecords declarationJson:declarationJson budgetMs:budgetMs ownerToken:self completion:^(NSString *value, NSString *failure) {
    if (failure != nil) { rejectWithFailure(reject, failure); return; } resolve(value);
  }];
}
- (void)orderedRecordingContext:(NSString *)sessionId id:(NSString *)recordingId resolve:(RCTPromiseResolveBlock)resolve reject:(RCTPromiseRejectBlock)reject {
  [[UnifiedBleRustCoreSessions shared] orderedRecordingControlWithSessionId:sessionId id:recordingId stop:NO ownerToken:self completion:^(NSString *value, NSString *failure) {
    if (failure != nil) { rejectWithFailure(reject, failure); return; } resolve(value);
  }];
}
- (void)orderedRecordingStop:(NSString *)sessionId id:(NSString *)recordingId resolve:(RCTPromiseResolveBlock)resolve reject:(RCTPromiseRejectBlock)reject {
  [[UnifiedBleRustCoreSessions shared] orderedRecordingControlWithSessionId:sessionId id:recordingId stop:YES ownerToken:self completion:^(NSString *value, NSString *failure) {
    if (failure != nil) { rejectWithFailure(reject, failure); return; } resolve(value);
  }];
}
- (void)orderedRecordingOffline:(NSString *)operation id:(NSString *)recordingId resolve:(RCTPromiseResolveBlock)resolve reject:(RCTPromiseRejectBlock)reject {
  [[UnifiedBleRustCoreSessions shared] orderedRecordingOfflineWithOperation:operation id:recordingId completion:^(NSString *value, NSString *failure) {
    if (failure != nil) { rejectWithFailure(reject, failure); return; } resolve(value);
  }];
}
- (void)orderedRecordingOfflineContext:(NSString *)recordingId resolve:(RCTPromiseResolveBlock)resolve reject:(RCTPromiseRejectBlock)reject {
  [self orderedRecordingOffline:@"context" id:recordingId resolve:resolve reject:reject];
}
- (void)orderedRecordingOfflineStatus:(NSString *)recordingId resolve:(RCTPromiseResolveBlock)resolve reject:(RCTPromiseRejectBlock)reject {
  [self orderedRecordingOffline:@"status" id:recordingId resolve:resolve reject:reject];
}
- (void)orderedRecordingOfflineStop:(NSString *)recordingId resolve:(RCTPromiseResolveBlock)resolve reject:(RCTPromiseRejectBlock)reject {
  [self orderedRecordingOffline:@"stop" id:recordingId resolve:resolve reject:reject];
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
- (void)notificationFailureSpoolConfigure:(NSString *)recordingId maxBytes:(NSString *)maxBytes maxRecords:(NSString *)maxRecords resolve:(RCTPromiseResolveBlock)resolve reject:(RCTPromiseRejectBlock)reject {
  [[UnifiedBleRustCoreSessions shared] notificationFailureSpoolConfigure:recordingId maxBytes:maxBytes maxRecords:maxRecords completion:^(NSString *value, NSString *failure) {
    if (failure != nil) { rejectWithFailure(reject, failure); return; }
    resolve(value);
  }];
}
- (void)scanFailureRecoveryOpenSelected:(RCTPromiseResolveBlock)resolve reject:(RCTPromiseRejectBlock)reject {
  [[UnifiedBleRustCoreSessions shared] scanFailureRecoveryOpenWithCompletion:^(NSString *value, NSString *failure) {
    if (failure != nil) { rejectWithFailure(reject, failure); return; } resolve(value);
  }];
}

- (void)terminalEvidenceRecoveryOpenSelected:(RCTPromiseResolveBlock)resolve reject:(RCTPromiseRejectBlock)reject {
  @try { resolve([[UnifiedBleRustCoreSessions shared] terminalEvidenceRecoveryOpenSelected]); }
  @catch (NSException *error) { reject(@"terminalEvidenceRecoveryOpenSelected", error.reason, error); }
}
- (void)terminalEvidenceRecoveryList:(NSString *)leaseId maxItems:(double)maxItems maxBytes:(double)maxBytes resolve:(RCTPromiseResolveBlock)resolve reject:(RCTPromiseRejectBlock)reject {
  @try {
    if (maxItems < 1 || floor(maxItems) != maxItems || maxItems > UINT32_MAX || maxBytes < 1 || floor(maxBytes) != maxBytes || maxBytes > UINT64_MAX) { @throw [NSException exceptionWithName:@"argument.invalid" reason:@"invalid terminal recovery bounds" userInfo:nil]; }
    resolve([[UnifiedBleRustCoreSessions shared] terminalEvidenceRecoveryList:leaseId maxItems:(uint32_t)maxItems maxBytes:(uint64_t)maxBytes]);
  } @catch (NSException *error) { reject(@"terminalEvidenceRecoveryList", error.reason, error); }
}
- (void)terminalEvidenceRecoveryListWithStatus:(NSString *)leaseId maxItems:(double)maxItems maxBytes:(double)maxBytes resolve:(RCTPromiseResolveBlock)resolve reject:(RCTPromiseRejectBlock)reject {
  @try {
    if (maxItems < 1 || floor(maxItems) != maxItems || maxItems > UINT32_MAX || maxBytes < 1 || floor(maxBytes) != maxBytes || maxBytes > UINT64_MAX) { @throw [NSException exceptionWithName:@"argument.invalid" reason:@"invalid terminal recovery bounds" userInfo:nil]; }
    resolve([[UnifiedBleRustCoreSessions shared] terminalEvidenceRecoveryListWithStatus:leaseId maxItems:(uint32_t)maxItems maxBytes:(uint64_t)maxBytes]);
  } @catch (NSException *error) { reject(@"terminalEvidenceRecoveryListWithStatus", error.reason, error); }
}
- (void)terminalEvidenceRecoveryRead:(NSString *)handle offset:(NSString *)offset maxBytes:(double)maxBytes resolve:(RCTPromiseResolveBlock)resolve reject:(RCTPromiseRejectBlock)reject {
  @try { if (maxBytes < 1 || floor(maxBytes) != maxBytes || maxBytes > UINT32_MAX) { @throw [NSException exceptionWithName:@"argument.invalid" reason:@"invalid terminal recovery read bound" userInfo:nil]; } resolve([[UnifiedBleRustCoreSessions shared] terminalEvidenceRecoveryRead:handle offset:offset maxBytes:(uint32_t)maxBytes]); }
  @catch (NSException *error) { reject(@"terminalEvidenceRecoveryRead", error.reason, error); }
}
- (void)terminalEvidenceRecoveryAcknowledge:(NSString *)handle resolve:(RCTPromiseResolveBlock)resolve reject:(RCTPromiseRejectBlock)reject {
  @try { resolve([[UnifiedBleRustCoreSessions shared] terminalEvidenceRecoveryAcknowledge:handle]); }
  @catch (NSException *error) { reject(@"terminalEvidenceRecoveryAcknowledge", error.reason, error); }
}
- (void)scanFailureRecoveryOwners:(NSString *)leaseId resolve:(RCTPromiseResolveBlock)resolve reject:(RCTPromiseRejectBlock)reject {
  [[UnifiedBleRustCoreSessions shared] scanFailureRecoveryOwners:leaseId completion:^(NSString *value, NSString *failure) {
    if (failure != nil) { rejectWithFailure(reject, failure); return; } resolve(value);
  }];
}
- (void)scanFailureRecoveryMember:(NSString *)ownerHandle index:(double)index resolve:(RCTPromiseResolveBlock)resolve reject:(RCTPromiseRejectBlock)reject {
  // The opaque owner handle encodes its lease scope in the native registry.
  [[UnifiedBleRustCoreSessions shared] scanFailureRecoveryMemberWithOwnerHandle:ownerHandle index:index completion:^(NSString *value, NSString *failure) {
    if (failure != nil) { rejectWithFailure(reject, failure); return; } resolve(value);
  }];
}
- (void)scanFailureRecoveryHead:(NSString *)ownerHandle index:(double)index resolve:(RCTPromiseResolveBlock)resolve reject:(RCTPromiseRejectBlock)reject {
  [[UnifiedBleRustCoreSessions shared] scanFailureRecoveryHeadWithOwnerHandle:ownerHandle index:index completion:^(NSString *value, NSString *failure) {
    if (failure != nil) { rejectWithFailure(reject, failure); return; } resolve(value);
  }];
}
- (void)scanFailureRecoveryControl:(NSString *)headHandle operation:(NSString *)operation resolve:(RCTPromiseResolveBlock)resolve reject:(RCTPromiseRejectBlock)reject {
  [[UnifiedBleRustCoreSessions shared] scanFailureRecoveryControlWithHeadHandle:headHandle operation:operation completion:^(NSString *value, NSString *failure) {
    if (failure != nil) { rejectWithFailure(reject, failure); return; } resolve(value);
  }];
}
- (void)scanFailureRecoveryPendingCleanup:(NSString *)leaseId resolve:(RCTPromiseResolveBlock)resolve reject:(RCTPromiseRejectBlock)reject {
  [[UnifiedBleRustCoreSessions shared] scanFailureRecoveryPendingCleanup:leaseId completion:^(NSString *value, NSString *failure) {
    if (failure != nil) { rejectWithFailure(reject, failure); return; } resolve(value);
  }];
}
- (void)scanFailureRecoveryRetireCleanup:(NSString *)cleanupReceiptId resolve:(RCTPromiseResolveBlock)resolve reject:(RCTPromiseRejectBlock)reject {
  [[UnifiedBleRustCoreSessions shared] scanFailureRecoveryRetireCleanupWithHandleId:cleanupReceiptId completion:^(NSString *failure) {
    if (failure != nil) { rejectWithFailure(reject, failure); return; } resolve(nil);
  }];
}
- (void)scanFailureRecoveryRawControl:(NSString *)ownerHandle operation:(NSString *)operation resolve:(RCTPromiseResolveBlock)resolve reject:(RCTPromiseRejectBlock)reject {
  [[UnifiedBleRustCoreSessions shared] scanFailureRecoveryRawControlWithOwnerHandle:ownerHandle operation:operation completion:^(NSString *value, NSString *failure) {
    if (failure != nil) { rejectWithFailure(reject, failure); return; } resolve(value);
  }];
}
- (void)scanFailureRecoveryRawInfo:(NSString *)ownerHandle resolve:(RCTPromiseResolveBlock)resolve reject:(RCTPromiseRejectBlock)reject {
  [[UnifiedBleRustCoreSessions shared] scanFailureRecoveryRawInfoWithOwnerHandle:ownerHandle completion:^(NSString *value, NSString *failure) {
    if (failure != nil) { rejectWithFailure(reject, failure); return; } resolve(value);
  }];
}
- (void)scanFailureRecoveryRawMember:(NSString *)ownerHandle index:(double)index resolve:(RCTPromiseResolveBlock)resolve reject:(RCTPromiseRejectBlock)reject {
  [[UnifiedBleRustCoreSessions shared] scanFailureRecoveryRawMemberWithOwnerHandle:ownerHandle index:index completion:^(NSString *value, NSString *failure) {
    if (failure != nil) { rejectWithFailure(reject, failure); return; } resolve(value);
  }];
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

- (void)prepareRetainedContinuationClaimV1:(double)maxItems
                                   maxBytes:(double)maxBytes
                                    resolve:(RCTPromiseResolveBlock)resolve
                                     reject:(RCTPromiseRejectBlock)reject {
  [[UnifiedBleRustCoreSessions shared] prepareRetainedContinuationClaimV1WithMaxItems:maxItems maxBytes:maxBytes
    completion:^(NSString *value, NSString *failure) {
      if (failure != nil) { rejectWithFailure(reject, failure); return; }
      resolve(value);
    }];
}

- (void)acknowledgeRetainedContinuationClaimV1:(NSString *)claimToken
                                      resolve:(RCTPromiseResolveBlock)resolve
                                       reject:(RCTPromiseRejectBlock)reject {
  [[UnifiedBleRustCoreSessions shared] acknowledgeRetainedContinuationClaimV1:claimToken
    completion:^(NSString *value, NSString *failure) {
      if (failure != nil) { rejectWithFailure(reject, failure); return; }
      resolve(value);
    }];
}

- (void)prepareRetainedContinuationClaimV2:(double)maxItems
                                   maxBytes:(double)maxBytes
                                    resolve:(RCTPromiseResolveBlock)resolve
                                     reject:(RCTPromiseRejectBlock)reject {
  [[UnifiedBleRustCoreSessions shared] prepareRetainedContinuationClaimV2WithMaxItems:maxItems maxBytes:maxBytes
    completion:^(NSString *value, NSString *failure) {
      if (failure != nil) { rejectWithFailure(reject, failure); return; }
      resolve(value);
    }];
}

- (void)acknowledgeRetainedContinuationClaimV2:(NSString *)claimToken
                                      resolve:(RCTPromiseResolveBlock)resolve
                                       reject:(RCTPromiseRejectBlock)reject {
  [[UnifiedBleRustCoreSessions shared] acknowledgeRetainedContinuationClaimV2:claimToken
    completion:^(NSString *value, NSString *failure) {
      if (failure != nil) { rejectWithFailure(reject, failure); return; }
      resolve(value);
    }];
}

- (void)readPeerFailureRecovery:(NSString *)recoveryRecord
                         offset:(NSString *)offset
                        resolve:(RCTPromiseResolveBlock)resolve
                         reject:(RCTPromiseRejectBlock)reject {
  [[UnifiedBleRustCoreSessions shared] readPeerFailureRecovery:recoveryRecord offset:offset
    completion:^(NSString *value, NSString *failure) {
      if (failure != nil) { rejectWithFailure(reject, failure); return; }
      resolve(value);
    }];
}

- (void)acknowledgePeerFailureRecovery:(NSString *)recoveryRecord
                               resolve:(RCTPromiseResolveBlock)resolve
                                reject:(RCTPromiseRejectBlock)reject {
  [[UnifiedBleRustCoreSessions shared] acknowledgePeerFailureRecovery:recoveryRecord
    completion:^(NSString *value, NSString *failure) {
      if (failure != nil) { rejectWithFailure(reject, failure); return; }
      resolve(value);
    }];
}

- (void)retireRetainedContinuationClaimReceiptV1:(NSString *)claimToken
                                        resolve:(RCTPromiseResolveBlock)resolve
                                         reject:(RCTPromiseRejectBlock)reject {
  [[UnifiedBleRustCoreSessions shared] retireRetainedContinuationClaimReceiptV1:claimToken
    completion:^(NSString *value, NSString *failure) {
      if (failure != nil) { rejectWithFailure(reject, failure); return; }
      resolve(value);
    }];
}

@end

#endif
