"""Generated UniFFI native RustBuffer roundtrip for owner retirement."""
import sys
sys.path.insert(0, sys.argv[1])
import ubm_echo as u

owner = "12345678-1234-4abc-8def-123456789abc"
retirement = "22345678-1234-4abc-8def-123456789abc"

def check(codec, value):
    assert codec.lift(codec.lower(value)) == value

for request in [u.MobileRadioRequest.OWNER_FENCE(10, owner, retirement),
                u.MobileRadioRequest.OWNER_STATUS(11, owner, retirement)]:
    check(u._UniffiFfiConverterTypeMobileRadioRequest, request)
first_failure = u.MobileOrderedFailureV2(kind="busy", gatt_status=7, native_domain="native", native_code=-42, native_name="first", detail="first retained failure", dispatched=False)
second_failure = u.MobileOrderedFailureV2(kind="unsupported", gatt_status=19, native_domain="CBATTErrorDomain", native_code=133, native_name="second", detail="second retained failure", dispatched=True)
for state in [u.MobileOwnerRetirementState.FENCED_PENDING(
                  [u.MobileOwnerOutstandingClass.CALLBACKS], [first_failure, second_failure]),
              u.MobileOwnerRetirementState.QUIESCENT(),
              u.MobileOwnerRetirementState.REJECTED(u.MobileOwnerRejection.WRONG_OWNER)]:
    check(u._UniffiFfiConverterTypeMobileRadioCompletion,
          u.MobileRadioCompletion.OWNER_RETIREMENT(owner, retirement, state))
print("generated UniFFI owner retirement v4 codecs: OK")

# Drive installation, callbacks, and completion through generated UniFFI,
# rather than treating the codec roundtrip as host integration evidence.
import json
import queue
import threading

class Radio:
    def __init__(self):
        self.versions = []
        self.requests = queue.Queue()
    def ordered_admission_owner(self, version):
        self.versions.append(version)
        return owner if version in (3, 4) else ""
    def submit(self, request):
        self.requests.put(request)
    def cancel(self, request_id):
        raise AssertionError(f"unexpected cancellation: {request_id}")

class Wake:
    def wake(self, session_id):
        raise AssertionError(f"unexpected wake: {session_id}")

radio = Radio()
host = u.mobile_host_install(radio, Wake(), "apple", "uniffi-v4-host", "synthetic")
assert radio.versions == [3, 4], radio.versions
results = queue.Queue()
def shutdown():
    try:
        results.put(json.loads(host.shutdown()))
    except BaseException as error:
        results.put(error)
worker = threading.Thread(target=shutdown)
worker.start()
while True:
    request = radio.requests.get(timeout=8)
    if request.is_CLOSE():
        assert host.complete(request.id, u.MobileRadioCompletion.CLOSED([])) == "delivered"
    elif request.is_OWNER_FENCE():
        assert request.owner_incarnation == owner
        completion = u.MobileRadioCompletion.OWNER_RETIREMENT(
            request.owner_incarnation, request.retirement_id,
            u.MobileOwnerRetirementState.QUIESCENT())
        assert host.complete(request.id, completion) == "delivered"
        break
    else:
        raise AssertionError(f"unexpected host request: {request}")
worker.join(timeout=8)
assert not worker.is_alive()
receipt = results.get_nowait()
assert isinstance(receipt, dict) and receipt["state"] == "released", receipt
print("generated UniFFI host independently negotiated and completed owner retirement: OK")
