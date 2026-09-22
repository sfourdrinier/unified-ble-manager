# Profiles RED note (follow-up to `4ea8d1ad`, review ACCEPT-WITH-FIXES)

## Claim disposition (LOW-5)

Commit `4ea8d1ad` says "Test-first: RED baseline recorded". That claim has no
artifact: the slice touches exactly `__tests__/profiles/profile-codecs.test.js`,
`crates/ubm-core/src/lib.rs`, and `crates/ubm-core/src/profiles.rs` (all
additive), and none of them records a RED run. `contracts/RED_BASELINE.md`
belongs to a different slice/commit. Retraction scope, precisely: the absence
of an artifact means the "RED baseline recorded" claim is unproven — not that
the tests were never run, only that no record exists to audit. This file is the
record for the follow-up slice: every new test below was executed against the
pre-fix code before the fix landed.

## Pre-fix run (RED)

Command (pre-fix tree = `fada1201` + new tests only, no implementation edits):

```
cargo test -p ubm-core profiles
```

Result: 12 passed, 1 failed.

- `profiles::tests::profile_selector_short_sig_uuids_resolve_like_helper` FAILED:
  `profile_selector("180D", "2A37", Some(0), Some(0))` piped into
  `Central::resolve_path` misses the canonical stored path (`gatt.not-found`
  inside `resolve_path`; the test panics at
  "short-uuid selector resolves through the central"). This reproduces
  MEDIUM-1 and went GREEN only after `profile_selector` canonicalizes both
  UUIDs and returns `Result`.
- All six new MEDIUM-2 negative-vector tests PASSED pre-fix
  (`ieee11073_special_values_decode`,
  `ieee11073_reserved_mantissas_reject_on_decode`,
  `ieee11073_negative_mantissa_round_trip_is_byte_exact`,
  `thermometer_negative_paths`, `blood_pressure_truncated_optional_tail`,
  `date_time_range_rejections`): the codecs already failed closed on these
  vectors, so these tests are characterization coverage for the previously
  untested decode paths — no defect found, reported exactly as observed.

Full pre-fix output:

```
running 13 tests
test profiles::tests::battery_level_round_trip_and_rejections ... ok
test profiles::tests::blood_pressure_and_device_information_vectors ... ok
test profiles::tests::blood_pressure_truncated_optional_tail ... ok
test profiles::tests::date_time_range_rejections ... ok
test profiles::tests::heart_rate_measurement_vectors ... ok
test profiles::tests::ieee11073_and_thermometer_vectors ... ok
test profiles::tests::ieee11073_negative_mantissa_round_trip_is_byte_exact ... ok
test profiles::tests::ieee11073_reserved_mantissas_reject_on_decode ... ok
test profiles::tests::ieee11073_special_values_decode ... ok
test profiles::tests::profile_paths_reject_duplicates_without_occurrence ... ok
test profiles::tests::profile_paths_resolve_through_real_central ... ok
test profiles::tests::profile_selector_short_sig_uuids_resolve_like_helper ... FAILED
test profiles::tests::thermometer_negative_paths ... ok

test result: FAILED. 12 passed; 1 failed; 0 ignored; 0 measured; 128 filtered out
```

## R-TCK4 RED/GREEN note

The two `subscriptionOptions('drop-oldest', 4, 32)` subscribes in
`src/tck/runner-public-scenarios.ts` (`executeSubscriptionSharingScenario`)
were invalid under the frozen R12 rule: `validateStreamLimits` rejects the
pre-fix `(4, 32)` budget with `stream.quota` (byte budget 32 <= 64-byte
control reserve) and admits the conformed `(4, 128)` budget — verified
directly against `contracts/src/streams.ts`. Same proof (no-value-before-ready,
shared CCCD refcount, consumer-isolated fanout), valid budgets.
