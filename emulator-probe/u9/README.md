# u9 — restart-recovery E2E battery (UBM 5.0)

Track: trackourhealth/bun-mono#1188 · gate U9 (fault/restart/migration).
Local lane only; nothing here is pushed by any slice.

## What this covers (6 records, 24/24 assertions green)

The durable restart contract end to end on a real emulator: the
foreground service persists the session-intent flag, process death keeps
it, reboot auto-restarts the service iff the flag is set, recovery fails
closed without the flag, and START_STICKY recreates the service after a
kill (null-intent path). Committed record: `results-u9.json` (raw
`logs/` stay local — gitignored, never committed).

1. **PRE** (REAL-EMULATOR): emulator reachable, example APK installs,
   MainActivity launches (out of stopped state).
2. **U9-1** (REAL-EMULATOR): headless service start (restartSticky=true)
   accepted, service running, flag persisted true, no FATAL.
3. **U9-2** (REAL-EMULATOR): force-stop kills the process; the flag
   survives death; relaunch clears stopped state (pre-reboot
   precondition — stopped apps never receive BOOT_COMPLETED).
4. **U9-3** (REAL-EMULATOR): reboot → service auto-restarts via the
   BOOT_COMPLETED recovery receiver, flag still true, no FATAL.
5. **U9-4** (REAL-EMULATOR): flag cleared + relaunch (so the receiver
   fires and must refuse) + reboot → service NOT restarted, no FATAL.
6. **U9-5** (REAL-EMULATOR): `am kill` → START_STICKY recreates the
   service through the null-intent path, flag still true, no FATAL.

Migration half (no emulator): the only durable 4.x store is the one
`unified-ble-manager` SharedPreferences boolean, and the 5.0 lane does
not change the restart path — so migration is a no-op by construction.
`__tests__/U9MigrationRehearsal.test.js` rehearses the read against a
copy of a 4.x prefs fixture (original byte-identical) and pins the
reader contract (file, key, fail-closed default, synchronous commits).
`RecoveryDecisionTest` (JVM) covers the extracted `shouldRecover`
decision matrix.

## Running it

```sh
# 1. Build the example APK (debug manifest carries the U9 scaffolding;
#    release APKs are untouched, src/main keeps the repo invariant):
cd example/android && ./gradlew :app:assembleDebug -PreactNativeArchitectures=x86_64
# 2. Run the battery (task-owned AVD ubm5-emu-u9 on emulator-5556,
#    temp home /tmp/ubm5-u9-avd; includes two reboots, ~8 min):
node emulator-probe/u9/run-u9-battery.js
# Results: u9/results-u9.json (committed) + u9/logs/ (local only)
```

`--only=PRE,U9-1` narrows the run (needs the serial up);
`--no-cleanup` leaves the owned emulator + AVD up for inspection
(default cleans them).

## Boundaries

- Debug-APK-only vehicle: the `src/debug` manifest exports the service
  for cross-uid starts and declares the receiver/metadata. Release
  posture (exported=false, host-declared integration) is unchanged.
- API 34 emulator; radio behavior is out of scope (no BLE peers).
- BOOT_COMPLETED delivery requires the app out of stopped state — the
  battery relaunches before every reboot (documented in-script).
