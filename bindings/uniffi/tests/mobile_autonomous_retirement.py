"""Exercise exact process-slot retirement through generated host bindings."""
import json
import queue
import sys
import threading
import time

sys.path.insert(0, sys.argv[1])
import ubm_echo as u

OWNER = "12345678-1234-4abc-8def-123456789abc"


class Radio:
    def __init__(self):
        self.requests = queue.Queue()

    def ordered_admission_owner(self, version):
        return OWNER if version in (3, 4) else ""

    def submit(self, request):
        self.requests.put(request)

    def cancel(self, request_id):
        raise AssertionError(f"unexpected cancellation {request_id}")


class Wake:
    def wake(self, session_id):
        raise AssertionError(f"unexpected wake {session_id}")


class Refuse:
    host = None

    def bind(self, host):
        self.host = host
        return False


def install(radio):
    return u.mobile_host_install(radio, Wake(), "apple", "replacement", "synthetic")


radio = Radio()
failed_install = len(sys.argv) > 2 and sys.argv[2] == "failed-install"
if failed_install:
    binding = Refuse()
    try:
        u.mobile_host_install_bound(radio, Wake(), binding, "apple", "failed", "synthetic")
        raise AssertionError("refused binding was installed")
    except u.MobileCoreError.Failed:
        pass
    host = binding.host
    shutdown = u.mobile_host_retry_pending_cleanup
else:
    host = install(radio)
    shutdown = host.shutdown

results = queue.Queue()
worker = threading.Thread(target=lambda: results.put(json.loads(shutdown())))
worker.start()
while True:
    request = radio.requests.get(timeout=8)
    if request.is_CLOSE():
        assert host.complete(request.id, u.MobileRadioCompletion.CLOSED([])) == "delivered"
    elif request.is_OWNER_FENCE():
        break
    else:
        raise AssertionError(f"unexpected request: {request}")
worker.join(timeout=5)
assert not worker.is_alive() and results.get_nowait()["state"] == "release-failed"
try:
    install(Radio())
    raise AssertionError("replacement admitted while retirement is unresolved")
except u.MobileCoreError.Failed:
    pass
assert host.complete(request.id, u.MobileRadioCompletion.OWNER_RETIREMENT(
    request.owner_incarnation, "22345678-1234-4abc-8def-123456789abc",
    u.MobileOwnerRetirementState.QUIESCENT())) == "mismatched"
assert (u.mobile_host_has_pending_cleanup() if failed_install else u.mobile_host_current() is not None)
assert host.complete(request.id, u.MobileRadioCompletion.OWNER_RETIREMENT(
    request.owner_incarnation, request.retirement_id, u.MobileOwnerRetirementState.QUIESCENT())) == "delivered"
deadline = time.monotonic() + 8
while time.monotonic() < deadline:
    if u.mobile_host_current() is None and not u.mobile_host_has_pending_cleanup():
        break
    time.sleep(0.02)
else:
    raise AssertionError("autonomous retirement did not clear the exact binding slot")
replacement = install(Radio())
assert replacement is not None
print("generated host autonomous exact binding retirement: OK")
