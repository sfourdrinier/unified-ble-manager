# Web Bluetooth TypeScript Example Design

## Goal

Turn the existing Web Bluetooth harness into a user-facing, framework-free TypeScript/Vite example and make `docs/WEB.md` a complete guide to the browser lifecycle.

## Scope

- Replace `example-web/app.js` and the repository-owned webpack build script with a Vite TypeScript application.
- Keep the example device-neutral by using the standard Heart Rate and Battery profiles.
- Demonstrate chooser activation, bounded connect/discover/read/subscribe operations, notifications, disconnect, authorized-peer reconnect, manager destruction, and structured `BleError` diagnostics.
- Treat a timed-out browser connection as still pending because `BluetoothRemoteGATTServer.connect()` is not cancellable. Destroy that manager before allowing a fresh retry; never retry immediately on the same manager.
- Explain secure contexts, Chrome/Chromium support, iframe Permissions Policy, service grants, origin-scoped authorization, the absence of continuous browser scanning/background restoration, and deterministic cleanup.
- Do not add device-specific protocols or change UBM runtime behavior.

## Example structure

- `example-web/src/main.ts` owns the UI lifecycle and public UBM calls.
- `example-web/src/style.css` provides a high-contrast responsive interface.
- `example-web/index.html` contains semantic application landmarks and controls.
- `example-web/vite.config.mts` resolves the example against the current repository package build.
- `example-web/tsconfig.json` compiles the example strictly with Web Bluetooth types.

The UI shows prerequisites, current stage, peer identity, battery and heart-rate values, structured error fields, operation history, and resource counters. All controls remain explicit user actions.

## Verification

Repository tests assert public imports, bounded operations, chooser/connect/discovery/read/notification coverage, safe timeout recovery, cleanup, and documentation. The Vite production build must pass against the current packed public entrypoints. The canonical repository package gate runs once after focused development checks.
