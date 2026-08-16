<!-- SUPPORT.md -->

# Support policy

## Package support versus backend qualification

`unified-ble-manager@4.0.0` establishes the stable 4.x package/API contract. Platform/backend support labels are a separate evidence-backed dimension rather than a static compatibility matrix.

Consult [`docs/PLATFORMS.md`](docs/PLATFORMS.md) and [`docs/generated/PLATFORM_SUPPORT.md`](docs/generated/PLATFORM_SUPPORT.md) before relying on a backend support label. `Experimental`, `Preview`, `Live Preview`, `Supported`, and `Reliability-qualified` describe the level of retained proof for that host/backend path.

An unavailable device lab lowers only the affected evidence label. It does not turn deterministic, compile, ABI, or package evidence into live-radio proof, and it does not make the host-neutral stable package prerelease.

## Getting help

Use **GitHub Issues in this repository** for:

- reproducible defects;
- host/backend integration problems;
- documentation errors;
- focused usage questions that can be answered from a concrete configuration/reproduction;
- backend-SDK conformance problems.

Use the private vulnerability-reporting path for suspected security issues; follow [`SECURITY.md`](SECURITY.md) and do not post vulnerability details publicly.

A good support report includes:

- exact `unified-ble-manager` version;
- public entrypoint used;
- backend/host (`react-native`, `web`, Electron main/renderer, CoreBluetooth, WinRT, BlueZ, custom backend);
- Node/Electron/React Native/Expo version as applicable;
- operating system and version;
- adapter and peripheral model when relevant;
- minimal reproduction;
- normalized error/terminal details;
- redacted diagnostics.

Do not post real BLE payloads, patient/user data, credentials, or stable device identifiers unless they are synthetic fixtures and necessary to reproduce the issue.

## Supported versions

Beginning with stable `4.0.0`, support targets the current 4.x release line. Security support follows [`SECURITY.md`](SECURITY.md).

Support is best-effort open-source maintenance. It does not replace application-specific device validation, regulatory review, safety engineering, or an emergency/medical monitoring system.
