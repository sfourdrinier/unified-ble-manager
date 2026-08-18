# Markdown-first 4.x consumer docs rewrite

> Execute on branch `docs/markdown-first-4x-rewrite`. Goal source of truth: the harness plan. This file is the working map.

**Goal:** Teach the current public 4.x API in human + LLM-usable markdown. No GitHub Pages in this pass.

**Voice:** What it does, why that shape helps, a typechecking snippet, what will hurt you. Do not lead with alpha-train history.

**Keep:** Imagi Explain sponsor line first. Lineage is an evolution of react-native-ble-plx and a cross-platform unified BLE-central product.

## Files

| File | Job |
| --- | --- |
| `__tests__/Docs.consumer.test.js` | Consumer docs match the published package |
| `__tests__/Package.identity.test.js` | Package name, exports, and migration map |
| `README.md` | Front door: sponsor, product, status, RN loop, method index, doc map |
| `docs/GETTING_STARTED.md` | 15-minute RN/Expo path + host chooser |
| `docs/TUTORIALS.md` | GATT recipes that typecheck |
| `docs/HELPERS.md` | Helper recipes that typecheck |
| `docs/WEB.md` `ELECTRON.md` `NODE.md` `TAURI.md` `EXPO_PLUGIN.md` | Host construction, real types |
| `docs/CONNECTION_MANAGER.md` | Reconnect is app-owned |
| `docs/PROFILES_AND_COMMANDS.md` | Keep export tables; human lead-in |
| `MIGRATION_4.0.md` | ble-plx 3.x cheat-sheet |
| example READMEs + construction sources | Same public contract |

## Known-wrong list (must stay gone)

- `/codecs` as Base64
- `device.name` (use `localName`)
- undefined `controlPointPath`
- unbranded scan/connect deadlines
- “no CoreBluetooth prebuild”
- Tauri `scan({ serviceUuids })` presented as `BleManager.scan`
- teaching-lead alpha / npm `next` on consumer host pages
- “4.0.0 is the first stable release” while `package.json` is an RC

## Order

1. Consumer-docs tests first (red).
2. README + GETTING_STARTED.
3. TUTORIALS / HELPERS / hosts.
4. MIGRATION_4.0.md.
5. Examples.
6. Honesty green + adversarial close-out.
