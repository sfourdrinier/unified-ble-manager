#import <React/RCTBridgeModule.h>

// App-only legacy-module interop is supported by React Native's new architecture.
// Both this registration and the Swift implementation belong to the app target.
@interface RCT_EXTERN_MODULE(UBMReferenceContinuation, NSObject)
RCT_EXTERN_METHOD(invoke:(NSString *)operation
                  peer:(NSString *)peer
                  declarationJson:(NSString *)declarationJson
                  token:(NSString *)token
                  maxItems:(double)maxItems
                  maxBytes:(double)maxBytes
                  resolve:(RCTPromiseResolveBlock)resolve
                  reject:(RCTPromiseRejectBlock)reject)
@end
