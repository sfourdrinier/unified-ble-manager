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
