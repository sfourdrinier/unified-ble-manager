# tauri-plugin-unified-ble-manager

Cross-platform Tauri v2 BLE plugin for Unified BLE Manager 4.0. It authenticates
the invoking webview/window in Rust, exposes one permission-gated command, and
ships a production `btleplug` dispatcher for CoreBluetooth, WinRT, and BlueZ.

The current consumer install uses the plugin and vendor patches from the same
packed npm package. Put these entries in the app's `src-tauri/Cargo.toml`:

```toml
[dependencies]
tauri = { version = "2", features = [] }
tauri-plugin-unified-ble-manager = { path = "../node_modules/unified-ble-manager/native/tauri" }

# Cargo reads patches only from the consuming workspace root.
[patch.crates-io]
btleplug = { path = "../node_modules/unified-ble-manager/vendor/btleplug" }
bluez-async = { path = "../node_modules/unified-ble-manager/vendor/bluez-async" }
```

The crate is not yet published. Cargo ignores patch tables in dependency
manifests. `ubm init --host tauri --dir src-tauri` generates this recipe;
the complete setup is in [`../../docs/TAURI.md`](../../docs/TAURI.md).

Register the production dispatcher:

```rust
tauri::Builder::default()
    .plugin(
        tauri_plugin_unified_ble_manager::PluginBuilder::new(
            tauri_plugin_unified_ble_manager::BtleplugDispatcher::default(),
        )
        .build(),
    )
```

When a host has more than one Bluetooth adapter, select one in trusted Rust
configuration with `BtleplugDispatcherOptions { adapter_id: Some(...), ..Default::default() }`. The
renderer cannot select or forge adapter authority. Custom deterministic or
platform-native dispatchers can implement `IpcDispatcher` without changing the
webview protocol.

Linux connections additionally require trusted Rust `connection_policy:
Some(tauri_plugin_unified_ble_manager::BluezConnectionPolicy::LeBearer { daemon_unique_owner })`
in those options. The host must verify that this current unique D-Bus owner of
`org.bluez` implements LE-only lifecycle methods; introspection alone is not
proof. Omission permits scanning, not connection acquisition. No device-wide
fallback is available, and a renderer cannot provide or replace the policy.
See the [BlueZ migration guidance](../../docs/NODE.md#bluez-connection-policy-bus-and-pairing-generation).

Grant `unified-ble-manager:default` only to the intended windows/webviews in the
application's Tauri capability file.

### Read-only peer directory

Authenticated `peers.resolve`, `peers.known`, `peers.connected`, and `peers.bonded`
routes use the same instantiated desktop central. CoreBluetooth connected retrieval requires
nonempty canonical service UUIDs; otherwise it returns `capability.unsupported`
with operation `peers.connected.services-required`. Known retrieval requires
explicit application-scoped `unified-ble:corebluetooth` references and rejects
service filters. Full UUID reference identifiers are case-normalized.

Windows and Linux expose `peers.bonded` through their native bond inventories.
Returned references have `scope: "application"`: Windows uses
`unified-ble:winrt` with a canonical uppercase Bluetooth address; Linux uses
`unified-ble:bluez-dbus` with its adapter-scoped `hciN/dev_AA_BB_CC_DD_EE_FF`
identity. `peers.resolve` accepts those returned references through the same
bonded inventory, preserving `system-bonded` as their source. Foreign backend
or reference scopes are rejected rather than rebound to another adapter.
CoreBluetooth does not expose unrestricted bond inventory and reports
`capability.unsupported` for `peers.bonded`.

Bonded retrieval accepts optional references but rejects nonempty service
filters with `capability.unsupported`, operation `peers.bonded.services`.
Source filters are applied after native lookup: an empty or excluding source list returns no records;
it does not bypass native capability checks or turn a native refusal into success.
Other categories and adapters without their native mechanisms report unsupported.

Lookup does not scan, connect, create a caller connection lease, or fabricate an
advertisement. The returned peer identity works with a subsequent explicit
connection request. All inputs are validated before lookup; batches retain the
original deadline/cancellation and attachment, and refuse publication after
adapter reset or caller retirement. Capabilities use the instantiated core's
registered states. Deterministic tests are not physical-radio qualification.

### Trusted durable-recording controls

Rust host code can configure an existing app-private directory with
`dispatcher.continuation_configure_recording_directory(path).await`. The host
owns its permissions, backup exclusion, and operating-system file protection;
the journal is plaintext (`encrypted: false`). Configuration is idempotent for
the same canonical directory and refuses replacement with a different one.

The trusted methods `continuation_recording_status(id)`,
`continuation_recording_prepare(id, max_items, max_bytes)`,
`continuation_recording_acknowledge(id, token)`,
`continuation_recording_stop(id)`, and `continuation_recording_clear(id)` use the
same native continuation registry. Their SQLite work runs on a blocking worker,
not the async runtime/UI thread. These are Rust methods, **not renderer commands**;
never expose private directory configuration to a webview.
Stored data can be inspected and exported before any Bluetooth authority is
initialized, including when radio access is unavailable.

Prepare returns a stable retained prefix; acknowledge it only after the
application has safely consumed/exported that prefix. Native claim/release does
not acknowledge durable records. Stop closes collection admission, not the
physical radio lease; perform native continuation claim/release separately.
Clear is explicitly destructive and requires a stopped recording.

An application may expose a separately authenticated, bounded bridge for these
operations using `native_continuation_envelope` and the renderer-side public
continuation codecs. Keep prepare and acknowledge separate: transport or decode
failure must not acknowledge a prefix. The checkout Tauri example demonstrates
this without accepting renderer paths or bootstrapping Bluetooth for journals.
Authoritative dispatcher shutdown stops recovery but retains its engine and
handoff. Failed cleanup remains retryable; even after confirmed parent release,
the caller must explicitly prepare/decode/acknowledge the retained handoff.
