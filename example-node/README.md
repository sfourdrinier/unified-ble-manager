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
```

A CLI has no app lifecycle, so the `background` scenario reports its app state
as `untracked`. Running this is a manual live check, not release evidence.
