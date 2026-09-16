// contracts/__tests__/version-axes.test.ts — C-UBM DRAFT U1 contract-freeze tests.
//
// Test-first: these tests freeze the CON-UBM version-axis requirements before
// the implementation below contracts/src exists. They must fail until the
// frozen C-UBM draft provides them.

import {
  BUILD_VERSION_IS_HANDSHAKE_AXIS,
  CONTRACT_ACCEPTANCE_GATE,
  CONTRACT_REVISION,
  CONTRACT_STATUS,
  assertContractRevisionEqual,
  assertHandshakeComplete,
  assertNegotiatedWithinOffer,
  makeVersionSpan,
  negotiateCoreOffer,
  negotiateIpcOffer,
  negotiateNativeOffer,
  negotiateVersionSpan,
} from '../src/index';

describe('C-UBM contract revision identity', () => {
  test('marks the draft revision and pending acceptance gate', () => {
    expect(CONTRACT_REVISION).toBe('C-UBM.0.1.2-DRAFT');
    expect(CONTRACT_STATUS).toBe('DRAFT');
    expect(CONTRACT_ACCEPTANCE_GATE).toBe('U1');
  });

  test('build/package version is never a handshake axis', () => {
    expect(BUILD_VERSION_IS_HANDSHAKE_AXIS).toBe(false);
  });
});

describe('version span negotiation (fail-closed)', () => {
  test('selects the highest common value', () => {
    const local = makeVersionSpan('backend-contract', 1, 3);
    const remote = makeVersionSpan('backend-contract', 2, 5);
    const negotiated = negotiateVersionSpan(local, remote);
    expect(negotiated.selected).toBe(3);
    expect(negotiated.axis).toBe('backend-contract');
  });

  test('rejects disjoint ranges before any effect', () => {
    const local = makeVersionSpan('backend-contract', 1, 1);
    const remote = makeVersionSpan('backend-contract', 2, 2);
    expect(() => negotiateVersionSpan(local, remote)).toThrow('protocol.incompatible');
  });

  test('rejects malformed ranges', () => {
    expect(() => makeVersionSpan('backend-contract', 3, 2)).toThrow('protocol.malformed');
    expect(() => makeVersionSpan('backend-contract', -1, 2)).toThrow('protocol.malformed');
    expect(() => makeVersionSpan('backend-contract', 1.5, 2)).toThrow('protocol.malformed');
  });

  test('rejects cross-axis negotiation', () => {
    const local = makeVersionSpan('backend-contract', 1, 2);
    const remote = makeVersionSpan('event-schema', 1, 2);
    expect(() => negotiateVersionSpan(local, remote)).toThrow('protocol.malformed');
  });

  test('rejects an unoffered selected version', () => {
    const local = makeVersionSpan('capability-schema', 1, 1);
    const remote = makeVersionSpan('capability-schema', 1, 3);
    const negotiated = negotiateVersionSpan(local, remote);
    const narrower = makeVersionSpan('capability-schema', 2, 3);
    expect(() => assertNegotiatedWithinOffer(negotiated, narrower)).toThrow('protocol.incompatible');
  });
});

describe('core/native/ipc handshake offers', () => {
  test('negotiates all four core axes', () => {
    const local = {
      backendContract: makeVersionSpan('backend-contract', 1, 1),
      capabilitySchema: makeVersionSpan('capability-schema', 1, 1),
      eventSchema: makeVersionSpan('event-schema', 1, 1),
      traceFormat: makeVersionSpan('trace-format', 1, 1),
    };
    const remote = {
      backendContract: makeVersionSpan('backend-contract', 1, 2),
      capabilitySchema: makeVersionSpan('capability-schema', 1, 1),
      eventSchema: makeVersionSpan('event-schema', 1, 1),
      traceFormat: makeVersionSpan('trace-format', 1, 1),
    };
    const negotiated = negotiateCoreOffer(local, remote);
    expect(negotiated.backendContract.selected).toBe(1);
    expect(negotiated.capabilitySchema.selected).toBe(1);
    expect(negotiated.eventSchema.selected).toBe(1);
    expect(negotiated.traceFormat.selected).toBe(1);
  });

  test('native and ipc axes negotiate independently', () => {
    const core = {
      backendContract: makeVersionSpan('backend-contract', 1, 1),
      capabilitySchema: makeVersionSpan('capability-schema', 1, 1),
      eventSchema: makeVersionSpan('event-schema', 1, 1),
      traceFormat: makeVersionSpan('trace-format', 1, 1),
    };
    const native = negotiateNativeOffer(
      { ...core, nativeProtocol: makeVersionSpan('native-protocol', 2, 2) },
      { ...core, nativeProtocol: makeVersionSpan('native-protocol', 1, 2) },
    );
    expect(native.nativeProtocol.selected).toBe(2);
    const ipc = negotiateIpcOffer(
      { ...core, ipcProtocol: makeVersionSpan('ipc-protocol', 1, 2) },
      { ...core, ipcProtocol: makeVersionSpan('ipc-protocol', 2, 2) },
    );
    expect(ipc.ipcProtocol.selected).toBe(2);
  });
});

describe('contract revision equality is never silent', () => {
  test('equal revisions pass', () => {
    expect(() => assertContractRevisionEqual('C-UBM.0.1.2-DRAFT', 'C-UBM.0.1.2-DRAFT')).not.toThrow();
  });

  test('different revisions fail closed', () => {
    expect(() => assertContractRevisionEqual('C-UBM.0.1.2-DRAFT', 'C-UBM.0.2.0-DRAFT')).toThrow(
      'protocol.incompatible',
    );
  });
});

describe('effects require a completed handshake (PKG-02)', () => {
  test('incomplete handshake rejects before effects', () => {
    expect(() => assertHandshakeComplete({ complete: false }, 'scan.start')).toThrow(
      'lifecycle.invalid-state',
    );
  });

  test('complete handshake admits effects', () => {
    expect(() => assertHandshakeComplete({ complete: true }, 'scan.start')).not.toThrow();
  });
});
