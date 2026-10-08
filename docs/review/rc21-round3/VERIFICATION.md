# Round-three independent verification

Release remains blocked. Three independent agents verified all 17 supplied
findings against `8f877ec839caa150a3f37d1fde8382f5273da04a`: one P1 and sixteen
P2 remain open. Eleven have fresh controlled reproductions; six have joined
production-source confirmation. The reviewed head was `ca8f66ad`; the only
implicated source file changed afterward was the Android radio, whose receiver
admission correction leaves the cited scan paths unchanged. All 74 files in
the supplied archive checksum manifest verified.

| Item  | Priority | Evidence             | Finding                                                                                 |
| ----- | -------- | -------------------- | --------------------------------------------------------------------------------------- |
| R3-01 | P1       | Controlled execution | Electron rollback can release a different operation's successful connection             |
| R3-02 | P2       | Controlled execution | Real Electron and Tauri initialization still disables implemented connection priority   |
| R3-03 | P2       | Controlled execution | Electron changes an accepted priority request into a timeout after completion           |
| R3-04 | P2       | Joined source        | WinRT physical loss makes retained discovery cleanup debt unreachable                   |
| R3-05 | P2       | Joined source        | Shutdown can race notification polling and strand an owned continuation queue           |
| R3-06 | P2       | Joined source        | Established Tauri parameter streams still replay old reports after a fresh gap snapshot |
| R3-07 | P2       | Controlled execution | A delayed readiness reprobe overwrites a newer live readiness event                     |
| R3-08 | P2       | Controlled execution | Android security cannot explicitly recover from a transient source failure              |
| R3-09 | P2       | Controlled execution | Transient parameter getter errors bar future watches after the getter recovers          |
| R3-10 | P2       | Controlled execution | Classic Pair contaminates LE ownership and leaves a false external origin               |
| R3-11 | P2       | Controlled execution | BlueZ ATT-handle parsers consume valid hexadecimal digits                               |
| R3-12 | P2       | Joined source        | Native notification callback failures never reach active public subscriptions           |
| R3-13 | P2       | Controlled execution | Android cached names continually renew absent advertisement local-name evidence         |
| R3-14 | P2       | Controlled execution | Android capture time and mobile source timestamps are discarded                         |
| R3-15 | P2       | Controlled execution | Public raw-advertisement opt-in unconditionally rejects supported Android bytes         |
| R3-16 | P2       | Joined source        | Windows PHY observation remains unimplemented despite the native getter                 |
| R3-17 | P2       | Joined source        | The new NAPI queue reset discards retained parameter-source failures                    |

R3-05 needs narrower impact wording: the shutdown admission race can publish a
premature terminal and omit queued values from the pre-cutoff retained claim.
Later disposal can drain and account the queue, so this review does not prove
a permanent memory leak or permanent resource stranding. It still requires a
barrier-based corrective regression.

Fresh Electron/IPC controls pass eight tests and 48 assertions; desktop
readiness/parameter controls pass three tests and 15 assertions; mobile controls
pass five tests and 46 assertions. Those tests intentionally assert defects and
positive controls. They are not corrective acceptance passes. Linux exact-body
C decisions also reproduce the Classic/LE contamination with a valid LE-only
positive control. Three standalone Rust intended-correctness parser tests fail,
confirming hexadecimal handle corruption; those failures are retained.

No physical radio, live daemon restart or native OS fault schedule was repeated
for this verification. Existing qualification limits remain unchanged, including
the Windows default MIDI profile and physical mobile/security retry limits.
The original 35-item ledger remains separate; this adds 17 open findings rather
than rewriting the previous denominator or claiming all prior work was undone.
CI on the final source revision remains necessary and cannot replace these
behavioral fixes.

The separate automated Web inclusion finding is corrected in `bca092e1`: only
native NotFoundError from the included-service getter becomes an observed empty
list. Unsupported getter metadata remains unknown, and other browser errors
remain failures. Forty-six Web tests, typecheck and docs pass, including the
production Navigator wrapper and failure controls. No native identity changed.

[Structured verification and source references](verification.json) binds every
R3 ID and retained receipt checksum.
