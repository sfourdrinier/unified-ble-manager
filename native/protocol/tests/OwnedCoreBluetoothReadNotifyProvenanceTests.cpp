// native/protocol/tests/OwnedCoreBluetoothReadNotifyProvenanceTests.cpp

#include "../include/OwnedCoreBluetoothReadNotifyProvenance.hpp"

#include <cassert>

namespace provenance = unified_ble::native_protocol::corebluetooth;

namespace {

void testIdleReadCompletes() {
  assert(!provenance::independentReadIsAmbiguous(false, false, false, false));
  assert(
      provenance::routeValueUpdate(true, false, false, false, false, false, true) ==
      provenance::ValueUpdateRoute::CompletePendingRead);
}

void testReadThenSubscribeDoesNotBecomeNotification() {
  assert(
      provenance::admitSubscribe(true, false) == provenance::SubscribeAdmission::RejectPendingRead);
  assert(
      provenance::routeValueUpdate(true, false, false, true, false, false, true) ==
      provenance::ValueUpdateRoute::CompletePendingRead);
  assert(
      provenance::routeValueUpdate(true, false, false, true, false, false, true) !=
      provenance::ValueUpdateRoute::DeliverNotification);
}

void testOverlappingReadsAreAmbiguousAtAdmission() {
  assert(provenance::independentReadIsAmbiguous(true, false, false, false));
  assert(provenance::independentReadIsAmbiguous(false, true, false, false));
  assert(provenance::independentReadIsAmbiguous(false, false, true, false));
  assert(provenance::independentReadIsAmbiguous(false, false, false, true));
}

void testCancelThenReadDropsFusedValue() {
  assert(
      provenance::routeValueUpdate(true, false, false, false, true, false, true) ==
      provenance::ValueUpdateRoute::RejectPendingRead);
  assert(
      provenance::routeValueUpdate(false, false, false, false, true, false, true) ==
      provenance::ValueUpdateRoute::Ignore);
}

void testOccurrenceAmbiguousNeverFallsThrough() {
  assert(provenance::occurrenceValueUpdateShouldReturn(true, false));
  assert(provenance::occurrenceValueUpdateShouldReturn(false, true));
  assert(!provenance::occurrenceValueUpdateShouldReturn(false, false));
}

void testPendingReadIsNotDeliveredAsNotification() {
  assert(
      provenance::routeValueUpdate(true, true, true, false, false, false, true) ==
      provenance::ValueUpdateRoute::RejectPendingRead);
}

}  // namespace

int main() {
  testIdleReadCompletes();
  testReadThenSubscribeDoesNotBecomeNotification();
  testOverlappingReadsAreAmbiguousAtAdmission();
  testCancelThenReadDropsFusedValue();
  testOccurrenceAmbiguousNeverFallsThrough();
  testPendingReadIsNotDeliveredAsNotification();
  return 0;
}
