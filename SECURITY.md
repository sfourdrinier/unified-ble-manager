<!-- SECURITY.md -->

# Security policy

## Reporting a vulnerability

Use this repository's **private vulnerability reporting / GitHub Security Advisory** flow when it is available. Do not put vulnerability details in a public issue, discussion, pull request, or commit before coordinated disclosure.

If the private reporting control is unexpectedly unavailable, open a public issue containing **no vulnerability details** and ask the maintainer for a private reporting channel.

A useful report includes:

- affected `unified-ble-manager` version;
- host/backend and operating-system/runtime versions;
- minimal reproduction steps;
- expected and observed security boundary;
- impact and realistic attacker prerequisites;
- whether physical proximity, a malicious BLE peripheral, renderer content, or a third-party backend is involved;
- any known mitigation.

Never include real patient data, device-owner data, production credentials, private keys, access tokens, or unnecessary persistent BLE identifiers.

The maintainer targets acknowledgement of a complete report within three business days and an initial severity/remediation plan within seven business days. These are response targets, not a guarantee that every investigation or fix will finish inside seven days.

## Supported versions

Beginning with `4.0.0`, the current 4.x release line receives security fixes. Older major lines are unsupported unless a release-specific notice says otherwise.

A security fix may remove or fail closed on unsafe behavior even when a compatibility path would be more convenient. Public disclosure should identify affected versions, mitigations, and fixed versions without exposing reporter-sensitive information.

## Security boundary

The package performs no telemetry or network upload by default. BLE identifiers and payloads can be sensitive. Consuming applications control their own storage, logging, analytics, backend trust, permissions, entitlements, and operating-system policy.

Host selection is a security boundary:

- the neutral root does not select a radio;
- Electron renderers do not own native radio access;
- third-party backends run with their host process privileges and must be reviewed as application dependencies;
- the library does not silently replace an unavailable selected backend with a different production radio implementation.

The maintained threat model is [`docs/security/UNIFIED_BLE_4.0_THREAT_MODEL.md`](docs/security/UNIFIED_BLE_4.0_THREAT_MODEL.md).

## Maintainer release requirement

Private vulnerability reporting must be enabled on the canonical `sfourdrinier/unified-ble-manager` repository before a stable release is published. This is a repository setting rather than a source-controlled file, so release preparation must verify it explicitly.
