<!-- docs/CLI.md -->

# `ubm` CLI

The CLI is non-interactive and emits one structured JSON result to standard output. It has no telemetry or network behavior. Consumer diagnosis commands do not select a radio. Backend-authoring commands import only the explicitly selected backend module and reject non-Node-capable providers before invoking a backend operation.

```text
ubm doctor
ubm doctor --json
ubm inspect package
ubm inspect config --host expo|tauri|electron|node|web
ubm inspect capabilities --host expo|tauri|electron|node|web
ubm init --host expo|tauri|electron|node|web [--dir <path>] [--force]
ubm support-bundle create --output <path> [--force]
ubm trace validate <file>
ubm trace redact <file>
ubm backend tck --backend <module>
ubm backend scenario --backend <module> --scenario <id>
ubm doctor --backend <module>
ubm capabilities --backend <module>
ubm tck --backend <module>
ubm scenario --backend <module> --scenario <id>
```

`ubm doctor` without `--backend` is read-only consumer diagnosis: installed package identity, runtime, Tauri npm/crate/protocol compatibility, and a `proofBoundary` of `compile-config-loadability`. It never reports live-radio evidence. `inspect package` is the same identity without adapter probing. `inspect config`/`inspect capabilities` report the documented install recipe and frozen compatibility metadata; they do not query crates.io, Cargo.lock, or a radio. `init` writes a reviewable public-API fragment and refuses to overwrite without `--force`. `support-bundle create` writes a local redacted JSON bundle and does not upload it. The crate recipe is `tauri-plugin-unified-ble-manager@4.0.0`; that crate is not yet published.

`--backend` forms still report declared provider/adapter state without inventing radio readiness. `capabilities --backend` projects the instantiated backend registry with its evidence and limitations. `tck`/`backend tck` runs every applicable base/feature suite; `scenario`/`backend scenario` verifies that the selected scenario appears in that complete truthful run. Trace commands are offline and bound input size to one MiB. `redact` prints a fresh redacted document and never overwrites the supplied input.

The backend module must export `unifiedBleBackend`, created through the public backend SDK. The CLI cannot and does not drive browser or React Native radio work.
