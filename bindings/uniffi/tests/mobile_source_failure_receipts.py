"""Generated UniFFI receipt replay against a synthetic mobile host, no radio."""
import sys
sys.path.insert(0, sys.argv[1])
import ubm_echo as u

class Radio:
    def ordered_admission_owner(self, version):
        return "12345678-1234-4abc-8def-123456789abc" if version == 3 else ""
    def submit(self, request):
        raise AssertionError(f"unexpected request: {request}")
    def cancel(self, request_id):
        raise AssertionError(f"unexpected cancellation: {request_id}")

class Wake:
    def wake(self, session_id):
        pass

host = u.mobile_host_install(Radio(), Wake(), "apple", "receipt-test", "synthetic")
assert host.notification_failure_ingress_version(1) == 1
assert host.notification_failure_ingress_version(2) == 0
instance = u.MobileInstance(peer_id="old", service_uuid="180d", service_occurrence=1,
    characteristic_uuid="2a37", characteristic_occurrence=2)
failure = u.MobilePlatformFailure(kind="platform", gatt_status=None, native_domain="CB", native_code=42,
    detail="original", dispatched=True)
changed = u.MobilePlatformFailure(kind="platform", gatt_status=None, native_domain="CB", native_code=43,
    detail="replacement", dispatched=True)
assert host.ingest_notification_failure_v1(0, instance, 7, failure) == "invalid"
assert host.ingest_notification_failure_v1(1, instance, 7, failure) == "accepted"
assert host.ingest_notification_failure_v1(1, instance, 7, failure) == "accepted"
assert host.ingest_notification_failure_v1(1, instance, 7, changed) == "invalid"
assert host.ingest_notification_failure_v1(2, instance, 7, changed) == "invalid"
assert host.ingest_notification_failure_v1(1, instance, 7, failure) == "accepted"
print("generated UniFFI source-failure receipt admission: OK (synthetic host)")
