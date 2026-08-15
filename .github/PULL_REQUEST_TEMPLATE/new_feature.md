---
name: 'New Feature'
about: 'Add a focused Unified BLE Manager capability.'
title: 'feat: '
labels: enhancement
---

## Problem / use case

Describe the consumer problem independently of a particular implementation.

## Proposed behavior

Describe the observable public behavior, affected host/backend(s), and why this belongs in the shared package rather than application/vendor code.

## Contract design

Cover the dimensions that apply:

- public TypeScript API and exports;
- backend/native protocol changes;
- byte/data ownership;
- cancellation/deadline semantics;
- manager/connection/GATT/subscription ownership and cleanup;
- typed errors and terminal events;
- runtime capabilities;
- Electron or other host security boundaries.

## Compatibility

- [ ] Backward-compatible within the current stable major version.
- [ ] Documentation/migration guidance is included for any consumer-visible behavior change.
- [ ] No silent production backend fallback is introduced.
- [ ] Platform/support claims remain limited to retained evidence.

If the proposal is breaking, explain why it requires a future major-version boundary rather than the current 4.x line.

## Validation

- [ ] Deterministic/TCK coverage added or updated.
- [ ] Host/native tests added where applicable.
- [ ] Documentation and generated artifacts updated where applicable.
- [ ] Actual physical-radio evidence is identified separately from compile/mock/deterministic proof.

## Alternatives considered

Describe simpler APIs, backend-local solutions, application-owned solutions, or other designs considered and why they are insufficient.
