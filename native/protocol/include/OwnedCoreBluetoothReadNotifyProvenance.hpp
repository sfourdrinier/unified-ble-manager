// native/protocol/include/OwnedCoreBluetoothReadNotifyProvenance.hpp

#pragma once

namespace unified_ble::native_protocol::corebluetooth {

inline constexpr int kIndependentReadIosCode = 1031;
inline constexpr int kIndependentReadElectronCode = 413;
inline constexpr int kOverlappingReadIosCode = 1011;
inline constexpr int kOverlappingReadElectronCode = 414;
inline constexpr int kSubscribeWhileReadIosCode = 1032;
inline constexpr int kSubscribeWhileReadElectronCode = 415;

enum class ValueUpdateRoute {
  CompletePendingRead,
  RejectPendingRead,
  DeliverNotification,
  Ignore
};

enum class SubscribeAdmission { Admit, RejectPendingRead, RejectPendingNotify };

inline bool independentReadIsAmbiguous(
    bool isNotifying,
    bool hasInstalledSubscription,
    bool pendingNotifyEnable,
    bool pendingCancellationCleanup) {
  return isNotifying || hasInstalledSubscription || pendingNotifyEnable || pendingCancellationCleanup;
}

inline SubscribeAdmission admitSubscribe(bool hasPendingRead, bool hasPendingNotify) {
  if (hasPendingRead) return SubscribeAdmission::RejectPendingRead;
  if (hasPendingNotify) return SubscribeAdmission::RejectPendingNotify;
  return SubscribeAdmission::Admit;
}

inline ValueUpdateRoute routeValueUpdate(
    bool hasPendingRead,
    bool isNotifying,
    bool hasInstalledSubscription,
    bool pendingNotifyEnable,
    bool pendingCancellationCleanup,
    bool hasError,
    bool hasValue) {
  if (hasPendingRead && !isNotifying && !hasInstalledSubscription && !pendingCancellationCleanup) {
    return ValueUpdateRoute::CompletePendingRead;
  }
  if (hasPendingRead) {
    return ValueUpdateRoute::RejectPendingRead;
  }
  if (!hasError && hasValue && (hasInstalledSubscription || pendingNotifyEnable) && !pendingCancellationCleanup) {
    return ValueUpdateRoute::DeliverNotification;
  }
  return ValueUpdateRoute::Ignore;
}

inline bool occurrenceValueUpdateShouldReturn(bool occurrenceAmbiguous, bool occurrenceStatePresent) {
  return occurrenceAmbiguous || occurrenceStatePresent;
}

}  // namespace unified_ble::native_protocol::corebluetooth
