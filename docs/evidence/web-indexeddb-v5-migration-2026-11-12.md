# Web IndexedDB v5 migration browser receipt — 2026-11-12

**Evidence level:** browser runtime integration for IndexedDB only. This is not
Bluetooth, native-radio, or production-deployment evidence.

## Scenario and result

The browser fixture
`__tests__/web/web-marker-ledger-upgrade-522-browser-fixture.js` opened real
IndexedDB databases at schema versions 2, 3, and 4, then opened the package's
version-5 journal through `openWebRecordingDatabase`. It retained and reread
the seeded legacy recording, record, and authority-reservation rows. For the
v4 case it also retained and reread rows in `markerLedger`,
`markerOperations`, `recoveryOperations`, and `abandonments`. Each upgraded
database contained the new `terminalEvidence` and `terminalEvidenceState`
stores. The fixture also exercised version-change closure, refusal of an old
version opener, and a blocked-upgrade fail-closed path.

Playwright observed this result from the page:

```
PASS real IndexedDB v2/v3/v4 upgrades preserve rows, versionchange, old writer fencing, blocked upgrade
```

## Reproduction

Bundle the fixture, serve a local HTML page that loads `fixture.js` as a module
from a fresh browser storage context, navigate Playwright to that page, then
wait up to 20 seconds for the exact PASS text above.

```sh
bun build __tests__/web/web-marker-ledger-upgrade-522-browser-fixture.js \
  --target=browser --outfile /tmp/ubm-idb-v5-playwright/fixture.js
```

The run used a local `node:http` server on `127.0.0.1:39151`; it made no
external-network, BLE, or device calls. The saved Playwright accessibility receipt was
`target/ubm-5-2-2-playwright-evidence/page-2026-11-12T09-37-21-982Z.yml` in the executing checkout
(an ignored local artifact). This document retains its exact observed outcome.

## Environment and identities

| Field | Value |
| --- | --- |
| Source commit | `9ba92ef70a173794e7ab98a7805eae75cb6df9f4` |
| Working-tree status digest | `ec412d79d83a8ab1d5a1c5a2579dc2f436cbfc355ddd78eaccb184442e327b45` |
| Fixture SHA-256 | `fa9092ecd866275e358dfea3e887e1b01fda5168a104a632b00d78c56e9ebe1f` |
| Host | macOS 26.6.2 |
| Browser | Google Chrome 157.0.8103.51, fresh Playwright context |
| Bundler | Bun 1.4.2 |
| Local server runtime | Node v24.21.0 |

## Limits

This receipt establishes only the recorded real-browser IndexedDB schema and
row-preservation scenario. It does not qualify Web Bluetooth, an actual radio,
other browsers, persistent production profiles, or a frozen release commit.
