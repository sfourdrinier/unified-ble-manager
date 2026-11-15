"""Generated Python codec and native RustBuffer round trips, without a radio."""
import sys
sys.path.insert(0, sys.argv[1])
import ubm_echo as u

maximum = 2**64 - 1
member = u.MobileOrderedMember(member_id=maximum, instance=u.MobileInstance(
    peer_id="Peer", service_uuid="service", service_occurrence=maximum,
    characteristic_uuid="characteristic", characteristic_occurrence=2**63))
spec = u.MobileOrderedOpenSpec(connection_generation="connection", database_generation="database",
    snapshot_identity=u.MobileGattSnapshotIdentity(owner="owner", attachment=maximum, revision=2**63),
    members=[member], item_capacity=2**32 - 1, byte_capacity=99)
identity = u.MobileOrderedOpenIdentity(version=2, owner_incarnation="12345678-1234-4abc-8def-123456789abc",
    slot=63, slot_generation=maximum, original_request_id=maximum, native_group_id=2**63,
    peer_id="Peer", spec=spec)
slot = u.MobileOrderedSlotIdentity(owner_incarnation=identity.owner_incarnation, slot=63, slot_generation=maximum)
failure = u.MobileOrderedFailureV2(kind="busy", gatt_status=7, native_domain="domain", native_code=-5,
    native_name="name", detail="original", dispatched=False)
registration = u.MobileOrderedRegistration(member_id=maximum, source_member=8, epoch=9)
outcomes = [u.MobileOrderedOpenOutcome.PENDING(),
    u.MobileOrderedOpenOutcome.OPENED(u.MobileOrderedGeneration.COUNTER(maximum), [registration]),
    u.MobileOrderedOpenOutcome.FAILED(failure, u.MobileOrderedOwnershipV2.CREATED(u.MobileOrderedGeneration.OPAQUE("source"), [registration])),
    u.MobileOrderedOpenOutcome.INDETERMINATE(u.MobileOrderedIndeterminateReason.FAILURE(failure))]

def check(codec, value):
    copy = codec.lift(codec.lower(value))
    assert copy == value, (copy, value)

for request in [u.MobileRadioRequest.ORDERED_OPEN_V2(maximum, identity),
                u.MobileRadioRequest.ORDERED_RECOVER_OPEN_V2(7, identity),
                u.MobileRadioRequest.ORDERED_ACKNOWLEDGE_OPEN_V2(8, identity)]:
    check(u._UniffiFfiConverterTypeMobileRadioRequest, request)
for outcome in outcomes:
    check(u._UniffiFfiConverterTypeMobileRadioCompletion, u.MobileRadioCompletion.ORDERED_OPEN_RESULT_V2(slot, outcome))
check(u._UniffiFfiConverterTypeMobileRadioCompletion, u.MobileRadioCompletion.ORDERED_OPEN_RETIRED_V2(slot))
native_failure = u.MobilePlatformFailure(kind="platform", gatt_status=None, native_domain="CB", native_code=257,
    detail="source failure", dispatched=False)
records = [u.MobileOrderedRecord.SOURCE_FAILED(True, native_failure, "CB", "257", registration, None),
           u.MobileOrderedRecord.SUBSEQUENT_SOURCE_FAILURE(native_failure),
           u.MobileOrderedRecord.FAILURE_OBSERVATIONS_OMITTED()]
check(u._UniffiFfiConverterTypeMobileRadioCompletion, u.MobileRadioCompletion.ORDERED_RECORDS(9, records))
print("generated Python ordered failure-summary records: OK")
print("generated Python v2 request and completion codecs: OK (native RustBuffer; no production caller)")
