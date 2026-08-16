---
name: 'Bug Fix'
about: 'Fix a reproducible Unified BLE Manager defect.'
title: 'fix: '
labels: bug
---

## Problem

Describe the defect, affected host/backend, and observable failure.

## Fix

Describe the behavioral change and why it addresses the root cause rather than masking the symptom.

## Contract / lifecycle impact

- [ ] No public API or backend-contract change.
- [ ] Public API/backend-contract change is documented and SemVer-compatible.
- [ ] Ownership, cancellation, teardown, late-completion, and capability semantics remain explicit.
- [ ] No production backend fallback or support claim was introduced implicitly.

Explain any checked item that needs context.

## Validation

- [ ] Added or updated deterministic regression coverage.
- [ ] Ran the focused tests for the changed area.
- [ ] Ran `pnpm validate:evidence`, `pnpm test:package`, `pnpm test:plugin`, and `pnpm lint` when applicable.
- [ ] Updated/regenerated documentation or release artifacts when applicable.
- [ ] Native/host-specific validation is identified below when the change touches native code.

### Host-specific proof

List the OS/runtime/device/build/ABI/live-radio validation actually performed. Do not describe compilation, mocks, or deterministic injection as physical-radio evidence.

## Compatibility / migration

Describe any consumer-visible compatibility or migration impact. If none, say `None`.
