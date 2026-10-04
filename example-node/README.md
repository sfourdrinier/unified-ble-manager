<!-- example-node/README.md -->

# Node desktop test-driver host

A small CLI that runs the cross-host test scenarios
([`../examples-shared/driver`](../examples-shared/driver/README.md)) in Node
through an explicit desktop entrypoint:
`unified-ble-manager/node/corebluetooth` (the macOS default),
`unified-ble-manager/node/winrt` (the Windows default) or
`unified-ble-manager/node/bluez` (the Linux default). The backend is selected by
`--backend` or by the OS default; nothing falls back to another backend.

```sh
pnpm prepack
node example-node/driver.ts serve-host                         # driven by the control server on ws://127.0.0.1:8795/host
node example-node/driver.ts run device-info read               # local: events as JSON lines
node example-node/driver.ts run h10-stream start --for 30000   # stream 30 s, then stop
node example-node/driver.ts serve-host --backend bluez --adapter /org/bluez/hci1  # exact public client adapter ID
```

Local `run` writes structured JSONL records exclusively to stdout. Human-readable
scenario diagnostics go to stderr, including their structured detail, so capture
both streams separately rather than filtering non-JSON lines out of evidence.
Local and server shutdown wait for both output streams' pending writes before
explicit exit, bounded to five seconds per drain attempt. Output failures make
the exit unsuccessful; native handles do not keep an otherwise completed CLI alive.

`--adapter` selects the one lazy process owner; scenario managers borrow that
same owner. Destroying a scenario manager does not stop an independently owned
native continuation. On a two-adapter fixture, select the client adapter, not the simulator's
adapter. Omitting it retains the public factory's normal adapter selection;
an explicitly empty value is rejected.

Shutdown attempts every scenario's cleanup, then process-owner cleanup even if
a scenario failed, and prints each outcome. A finite `run` exits nonzero with
explicit unresolved-cleanup diagnostics; this is not proof of release. A
`serve-host` with failed cleanup stays alive with the same owner and its remote
command admission stopped; send SIGINT or SIGTERM again to retry. Successful
shutdown still flushes stdout/stderr before exit.

Initialization errors still reach the requesting scenario. If the factory has
confirmed compensation, no nonexistent owner is retained and a later command
may retry admission. Failed compensation instead retains the exact cleanup
handle; shutdown retries it without opening another radio.

`process-continuation` exercises the native engine on this same process-owned
central. The process must remain alive; this does not register an OS relaunch
service. Journal commands use offline storage access without opening a radio.
Storage defaults to the OS application-data directory under
`unified-ble-manager/example-node/<backend>` (macOS Application Support, Windows
LOCALAPPDATA, or Linux XDG_DATA_HOME / `~/.local/share`). Trusted launch
configuration may set `UBM_RECORDINGS_DIRECTORY` to an absolute private path;
scenario arguments cannot select filesystem paths. Data is plaintext under OS
filesystem protections. Live recording execution configures that same directory
before the native order runs. Neither host shutdown nor native backlog claim
acknowledges or clears durable journal records; use explicit journal handoff
commands and retain returned data before acknowledging.

A CLI has no app lifecycle, so the `background` scenario reports its app state
as `untracked`. Running this is a manual live check, not release evidence.
# Trusted BlueZ daemon policy

For Linux, `--backend bluez` uses the maintained daemon integration described in
[`BLUEZ_DEPLOYMENT.md`](../docs/BLUEZ_DEPLOYMENT.md). The native authority resolves and pins
the daemon owner by default, and validates its lease/GATT contract; no application
lookup is required. Installation remains an explicit host action.

`--bluez-daemon-owner :1.N` or trusted `UBM_BLUEZ_DAEMON_OWNER=:1.N` adds an
optional stricter owner restriction (the CLI flag wins), not implementation
attestation. The public factory validates the unique name and refuses a mismatch
or replacement; a deliberately pinned restart needs updated trusted configuration.
Other backends reject this option. No Device1/legacy fallback exists. Ordinary
managers and process continuation share native authority; offline recordings do
not acquire a radio. Scenarios cannot set this policy.
