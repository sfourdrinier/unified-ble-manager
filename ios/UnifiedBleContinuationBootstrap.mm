// The standing order belongs to the native process, not a TurboModule.
// Register before launch, but allocate no radio until launch completes and
// the application has explicitly configured restoration (record-only by
// default) or native continuation. A standing order is not required to receive
// and retain restoration callbacks before JavaScript starts.
#import <Foundation/Foundation.h>
#import <UIKit/UIKit.h>
#import <CoreBluetooth/CoreBluetooth.h>

#if __has_include("BlePlx-Swift.h")
#import "BlePlx-Swift.h"

@interface UnifiedBleContinuationBootstrap : NSObject
@end

@implementation UnifiedBleContinuationBootstrap
+ (void)load {
  [NSNotificationCenter.defaultCenter addObserver:self
                                        selector:@selector(applicationDidFinishLaunching:)
                                            name:UIApplicationDidFinishLaunchingNotification
                                          object:nil];
}

+ (void)applicationDidFinishLaunching:(NSNotification *)notification {
  id identifiers = notification.userInfo[UIApplicationLaunchOptionsBluetoothCentralsKey];
  NSMutableArray<NSString *> *restorationIdentifiers = [NSMutableArray new];
  if ([identifiers isKindOfClass:NSArray.class]) {
    for (id identifier in identifiers) {
      if ([identifier isKindOfClass:NSString.class]) [restorationIdentifiers addObject:identifier];
    }
  }
  [UnifiedBleRustCoreSessions recordNativeRestorationLaunchIdentifiers:restorationIdentifiers];
  NSString *failure = [[UnifiedBleRustCoreSessions shared] bootstrapNativeContinuation];
  if (failure != nil) {
    NSLog(@"[UnifiedBleRustCore] native continuation bootstrap failed: %@", failure);
  }
}
@end
#endif
