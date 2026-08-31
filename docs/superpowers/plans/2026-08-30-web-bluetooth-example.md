# Web Bluetooth TypeScript Example Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship a complete TypeScript/Vite Web Bluetooth example and user-centric browser guide using UBM 4.0.10's current public API.

**Architecture:** Replace the single JavaScript harness with a strict TypeScript Vite app that imports only public package entrypoints. Keep browser lifecycle ownership in one small controller, use explicit operation deadlines, and destroy the manager after an uncancellable pending-connect timeout before retrying.

**Tech Stack:** TypeScript, Vite, Web Bluetooth, unified-ble-manager public Web and profile entrypoints, Jest documentation/source contract tests.

---

### Task 1: Pin the new example contract

**Files:**

- Modify: `__tests__/Web4.0Example.test.js`
- Modify: `__tests__/LegacyArchitectureAbsence.test.js`
- Modify: `__tests__/CiRelease.canonicalPackage.test.js`

- [ ] Assert that the source is `example-web/src/main.ts`, uses public imports, supplies positive finite `timeoutMs` values, renders structured error diagnostics, destroys the manager after pending connect timeout, and retains chooser/read/notification/cleanup controls.
- [ ] Run `pnpm jest --config jest.config.js __tests__/Web4.0Example.test.js __tests__/LegacyArchitectureAbsence.test.js __tests__/CiRelease.canonicalPackage.test.js --runInBand` and confirm the assertions fail against the JavaScript/webpack example.

### Task 2: Build the TypeScript/Vite example

**Files:**

- Delete: `example-web/app.js`
- Delete: `scripts/examples/build-web-example.js`
- Create: `example-web/src/main.ts`
- Create: `example-web/src/style.css`
- Create: `example-web/vite.config.mts`
- Create: `example-web/tsconfig.json`
- Modify: `example-web/index.html`
- Modify: `example-web/README.md`
- Modify: `package.json`
- Modify: `pnpm-lock.yaml`

- [ ] Add Vite as a root development dependency and make `build:example:web` run a strict TypeScript check followed by `vite build`.
- [ ] Implement the chooser directly in the click handler, then bounded connect, discovery, Battery read, Heart Rate subscription, notification consumption, disconnect, authorized reconnect, and manager destruction.
- [ ] Normalize caught values to `BleError` display fields without hiding the original code, domain, operation, or browser error name.
- [ ] On `operation.timed-out` from `web-connection.connect`, destroy the manager before enabling a fresh choose/retry action.
- [ ] Run the focused Jest tests and `pnpm build:example:web`; confirm both pass with zero diagnostics.

### Task 3: Write the complete Web Bluetooth guide

**Files:**

- Modify: `docs/WEB.md`
- Modify: `README.md`

- [ ] Document prerequisites and browser security boundaries.
- [ ] Provide a complete TypeScript lifecycle example and link to `example-web/`.
- [ ] Explain filters versus `optionalServices`, transient activation, `peers.authorized()`, origin authorization versus bonding, per-operation deadlines, notification streams, iframe policy, background limitations, cleanup, and safe timeout retry behavior.
- [ ] Correct the existing false statement that UBM does not expose authorized browser peers.
- [ ] Run `pnpm docs:check` and the focused consumer documentation tests.

### Task 4: Verify and deliver

**Files:**

- Modify only files already listed if a blocking verification defect is found.

- [ ] Run `pnpm build:example:web`.
- [ ] Run `pnpm test:package` once as the canonical JavaScript/package gate.
- [ ] Run `pnpm lint` and `pnpm docs:check`.
- [ ] Run `git diff --check` and confirm generated `example-web/dist/` is not staged.
- [ ] Commit the cohesive documentation/example hotfix and push directly to `origin/main`.
