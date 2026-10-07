# Final runtime batch and retained failures

The manifest records source and archive identities separately. The canonical
0d5f4cb9 run passes all 18 non-publishing jobs. The later 9916500f RN-only change
passes clean Linux preflight (430 suites / 5,415 tests), all four cross-platform
JS lanes, three packed Node/Bun lanes and three Tauri lanes. Its full Rust CI
fails: the portable registry test lacks its logging dev-dependency and Windows
Clippy rejects a redundant iterator conversion. Both corrections are collected
in the final gate batch; this receipt does not establish their success.

Windows uses the actual USB radio in the Windows VM and a simulated Linux RF
peripheral. Node CJS/ESM and Bun CJS pass the default 14-service profile. Bun ESM
passes the 13-service profile with BlueZ's built-in MIDI plugin temporarily
excluded. The library continues to discover all descriptors. Default-profile
Bun ESM can time out in encrypted MIDI user-description initialization; public
cleanup then returns a prompt, structured retirement-pending refusal, retaining
native ownership. This OS/profile interoperability delay remains a limitation.
The attempted pending-loss-retry control fails its coordinator deadline and
proves neither natural-completion retry nor zero resources. Its failure remains
retained. The runtime daemon override was removed and the original profile restored.

Real SMP held Pair success passes through the before-first-lease path. Final
lease release/ACK protects accepted pending Pair; successful Pair commits foreign
interest. Because the link was created by the foreign owner, it remains protected
until that owner explicitly disconnects. The first fixture incorrectly expected
a library-forced disconnect after sender death and is retained as a failed test.
The corrected fixture follows the foreign-origin contract and passes. Cleanup
removes only the two bonds created by this test, restores both Pairable flags,
and preserves the pre-existing user bond. USB controllers are real; the GATT
peripheral application is simulated. No physical H10 or mobile qualification is inferred.

The 92b89ce6 collected package run exposes 13 failures across five suites:
eight non-finite transport values throw before the stream guard, four dependency
artifact checks are stale after the logging dev-dependency, and one pinned
vocabulary table was reformatted. The follow-up preserves serialization failures
as owned stream terminals and regenerates both artifacts with their owners.
All failures remain retained; only the subsequent collected run can establish closure.

Final collected clean preflight at `10589aee` passes all Linux package/Tauri jobs,
including 430 suites / 5,433 package tests and 7 suites / 67 plugin tests. All 18
metadata regressions pass. The 4362c001 serialization terminal exposed blocked
dead-pump cleanup; 577d7a8e corrects that and exposes four stale old assertions.
Those assertions now require actual failed-release receipts, routed cleanup and
retained retry debt; 10589aee passes them. Each failed batch remains retained.
Cross-platform final readiness belongs to the current PR checks, not this Linux
receipt; Android Gradle builds were excluded by --fast and remain required in CI.

Post-candidate automated review: assertion-free IPC guards and invalidated Apple
child discovery retirement. `post-candidate-typecheck.log` passes;
`post-candidate-ipc.log` passes 111 tests across three suites, including malformed
metadata and owned cleanup; `post-candidate-apple.log` passes the complete native
Apple protocol/Swift/Rust harness, including actual production invalidation and
callback reservation controls with mutable CoreBluetooth fixture objects. This
is deterministic execution, not physical-radio evidence. No BlueZ, WinRT,
Android runtime or native Rust identity input changed in this follow-up.
