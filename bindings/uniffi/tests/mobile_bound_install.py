"""Exercise installation and retained failed binding through generated UniFFI."""
import sys
import threading
import json

sys.path.insert(0, sys.argv[1])
import ubm_echo as u

assert hasattr(u, "mobile_host_install_bound"), "a host-bound install must exist before it can be published"


class Radio:
    def ordered_admission_owner(self, version):
        assert version in (3, 4)
        return "12345678-1234-4abc-8def-123456789abc" if version == 3 else ""

    def submit(self, request):
        raise AssertionError(f"unexpected request: {request}")

    def cancel(self, request_id):
        raise AssertionError(f"unexpected cancellation: {request_id}")


class Wake:
    def wake(self, session_id):
        raise AssertionError(f"unexpected wake: {session_id}")


class RefusingBinding:
    host = None

    def bind(self, host):
        assert u.mobile_host_current() is None, "unbound host was published"
        self.host = host
        return False


if len(sys.argv) > 2 and sys.argv[2] == "invalid-owner":
    try:
        u.mobile_host_install_bound(Radio(), Wake(), RefusingBinding(), "apple", "", "test")
        raise AssertionError("empty host owner accepted")
    except u.MobileCoreError.Failed as error:
        assert error.code == "argument.invalid"
    assert not u.mobile_host_has_pending_cleanup(), "validation before construction created cleanup debt"
    assert u.mobile_host_current() is None
    assert json.loads(u.mobile_host_retry_pending_cleanup())["state"] == "release-failed"
    print("generated UniFFI validates host owner before construction: OK")
    sys.exit(0)


if len(sys.argv) > 2 and sys.argv[2] == "throw":
    class ThrowingBinding:
        def bind(self, host):
            assert u.mobile_host_current() is None
            raise ValueError("binding callback failed")

    try:
        u.mobile_host_install_bound(Radio(), Wake(), ThrowingBinding(), "apple", "test", "test")
        raise AssertionError("throwing binding was accepted")
    except u.MobileCoreError.Failed as error:
        assert error.code == "lifecycle.invalid-state"
    assert u.mobile_host_current() is None and u.mobile_host_has_pending_cleanup()
    assert '"state":"release-failed"' in u.mobile_host_retry_pending_cleanup()
    print("generated UniFFI throwing binding remains retained: OK")
    sys.exit(0)


if len(sys.argv) > 2 and sys.argv[2] == "held":
    entered = threading.Event()
    release = threading.Event()
    result = []

    class HeldBinding:
        def bind(self, host):
            assert u.mobile_host_current() is None
            assert json.loads(u.mobile_host_retry_pending_cleanup())["state"] == "release-failed"
            assert host.complete(987654321, u.MobileRadioCompletion.UNIT()) == "late"
            try:
                host.open_session("early", u.mobile_wire_revision())
                raise AssertionError("unpublished host admitted a session")
            except u.MobileCoreError.Failed as error:
                assert error.code == "lifecycle.invalid-state"
            refused = json.loads(host.continuation_reserve_declaration("{}"))
            assert refused["error"]["code"] == "lifecycle.invalid-state"
            blocked_shutdown = json.loads(host.shutdown())
            assert blocked_shutdown["state"] == "release-failed"
            assert blocked_shutdown["failures"][0]["detail"] == "binding in progress"
            entered.set()
            assert release.wait(10), "held binding was never released"
            return True

    def install():
        try:
            result.append(u.mobile_host_install_bound(Radio(), Wake(), HeldBinding(), "apple", "held", "test"))
        except BaseException as error:
            result.append(error)

    worker = threading.Thread(target=install)
    worker.start()
    assert entered.wait(10), "binding callback did not start"
    assert u.mobile_host_current() is None, "held binding was published early"
    try:
        u.mobile_host_install_bound(Radio(), Wake(), RefusingBinding(), "apple", "competing", "test")
        raise AssertionError("competing installation was accepted")
    except u.MobileCoreError.Failed as error:
        assert error.code == "lifecycle.invalid-state"
    release.set()
    worker.join(10)
    assert not worker.is_alive(), "installation did not finish"
    assert len(result) == 1 and not isinstance(result[0], BaseException), result
    assert u.mobile_host_current() is not None and not u.mobile_host_has_pending_cleanup()
    session = result[0].open_session("session", u.mobile_wire_revision())
    assert session.session_id() > 0
    try:
        session.negotiate_scan_end_version_qualified("[2]")
        raise AssertionError("unqualified v2 was selected")
    except u.MobileCoreError.Failed as error:
        assert error.code == "protocol.incompatible"
    assert json.loads(session.negotiate_scan_end_version_qualified("[1,2]"))["version"] == 1
    try:
        session.negotiate_scan_end_version("[2]")
        raise AssertionError("selected version changed")
    except u.MobileCoreError.Failed as error:
        assert error.code == "protocol.incompatible"
    assert '"state":"release-failed"' in result[0].shutdown()
    assert u.mobile_host_current() is not None
    print("generated UniFFI held binding stays unpublished until acceptance: OK")
    sys.exit(0)


binding = RefusingBinding()
try:
    u.mobile_host_install_bound(Radio(), Wake(), binding, "apple", "test", "test")
    raise AssertionError("refused binding was accepted")
except u.MobileCoreError.Failed as error:
    assert error.code == "lifecycle.invalid-state"
assert binding.host is not None
try:
    binding.host.open_session("refused", u.mobile_wire_revision())
    raise AssertionError("refused host admitted a session")
except u.MobileCoreError.Failed as error:
    assert error.code == "lifecycle.invalid-state"
assert u.mobile_host_current() is None
assert u.mobile_host_has_pending_cleanup()
for _ in range(2):
    assert '"state":"release-failed"' in u.mobile_host_retry_pending_cleanup()
    assert u.mobile_host_has_pending_cleanup()
    assert u.mobile_host_current() is None
    assert binding.host.complete(987654321, u.MobileRadioCompletion.UNIT()) == "late"
try:
    u.mobile_host_install_bound(Radio(), Wake(), RefusingBinding(), "apple", "replacement", "test")
    raise AssertionError("replacement accepted while old cleanup remains")
except u.MobileCoreError.Failed as error:
    assert error.code == "lifecycle.invalid-state"
print("generated UniFFI refused binding remains unpublished and retryable: OK")
