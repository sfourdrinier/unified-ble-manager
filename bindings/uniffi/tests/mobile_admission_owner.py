"""Exercise the generated UniFFI callback and real host installation without a radio."""
import sys
sys.path.insert(0, sys.argv[1])
import ubm_echo

class Radio:
    def __init__(self, identity, supported=3):
        self.identity = identity
        self.supported = supported
        self.versions = []
    def ordered_admission_owner(self, version):
        assert ubm_echo.mobile_host_current() is None, "installation callback must be able to inspect host state"
        try:
            ubm_echo.mobile_host_install(self, Wake(), "apple", "nested", "nested")
            raise AssertionError("nested installation was accepted")
        except ubm_echo.MobileCoreError.Failed as error:
            assert error.code == "lifecycle.invalid-state"
        self.versions.append(version)
        return self.identity if version == self.supported else ""
    def submit(self, request):
        raise AssertionError("unexpected native radio request")
    def cancel(self, request_id):
        raise AssertionError("unexpected cancellation")

class Wake:
    def wake(self, session_id):
        raise AssertionError("unexpected wake")

for invalid in ("", "not-a-uuid", "00000000-0000-0000-0000-000000000000"):
    radio = Radio(invalid)
    try:
        ubm_echo.mobile_host_install(radio, Wake(), "apple", "test", "test")
        raise AssertionError("invalid owner accepted")
    except ubm_echo.MobileCoreError.Failed as error:
        assert error.code == ("protocol.incompatible" if invalid == "" else "protocol.malformed")
        assert error.domain == "boundary"
        assert radio.versions == [3]
        assert ubm_echo.mobile_host_current() is None

legacy = Radio("12345678-1234-4abc-8def-123456789abc", supported=2)
try:
    ubm_echo.mobile_host_install(legacy, Wake(), "apple", "test", "test")
    raise AssertionError("v2-only owner accepted")
except ubm_echo.MobileCoreError.Failed as error:
    assert error.code == "protocol.incompatible"
    assert legacy.versions == [3]

class ConflictingRadio(Radio):
    def ordered_admission_owner(self, version):
        if version == 4:
            self.versions.append(version)
            return "12345678-1234-4abc-8def-123456789abd"
        return super().ordered_admission_owner(version)

conflicting = ConflictingRadio("12345678-1234-4abc-8def-123456789abc")
try:
    ubm_echo.mobile_host_install(conflicting, Wake(), "apple", "test", "test")
    raise AssertionError("different v4 owner was accepted")
except ubm_echo.MobileCoreError.Failed as error:
    assert error.code == "protocol.malformed"
    assert conflicting.versions == [3, 4]
    assert ubm_echo.mobile_host_current() is None

radio = Radio("12345678-1234-4abc-8def-123456789abc")
host = ubm_echo.mobile_host_install(radio, Wake(), "apple", "test", "test")
assert radio.versions == [3, 4]
assert ubm_echo.mobile_host_current() is not None
# Generated Python lowers this v2 result across the actual host FFI. The
# host has no matching request and must not accept retirement.
slot = ubm_echo.MobileOrderedSlotIdentity(owner_incarnation="12345678-1234-4abc-8def-123456789abc", slot=3, slot_generation=1)
assert host.complete(987654321, ubm_echo.MobileRadioCompletion.ORDERED_OPEN_RETIRED_V2(slot)) == "late"
try:
    ubm_echo.mobile_host_install(radio, Wake(), "apple", "test", "test")
    raise AssertionError("competing process host accepted")
except ubm_echo.MobileCoreError.Failed:
    pass
for _ in range(2):
    assert '"state":"release-failed"' in host.shutdown()
    assert ubm_echo.mobile_host_current() is not None
    assert host.complete(987654321, ubm_echo.MobileRadioCompletion.UNIT()) == "late"
    status = host.ingest(ubm_echo.MobileRadioIngress.ORDERED_WAKE(
        ubm_echo.MobileOrderedGeneration.COUNTER(1), 987654321))
    assert status in ("accepted", "dropped-control", "closed"), status
print("generated UniFFI admission callback: invalid owners rejected and valid owner installed")
