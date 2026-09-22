// contracts/__tests__/contract-fix-rulings.test.ts — U1 review follow-up (R1–R5, R8–R9, R11–R14).
// Each test pins one orchestrator ruling; all must fail on pre-fix code (see RED_BASELINE.md).
// No casts: JS-reachable inputs go through `any`-typed helpers so the same
// file compiles pre- and post-fix while still exercising runtime validation.

import {
  APPROVED_CORRECTIONS,
  BUILT_IN_CAPABILITY_IDS,
  CAPABILITY_STATES,
  CENTRAL_CONTROLS,
  CONTENTION_RULINGS,
  EARLY_EXIT_CLEANUP,
  ERROR_CODE_LIST,
  ERROR_DOMAIN_LIST,
  HAPPENS_BEFORE,
  MANDATORY_SCENARIOS,
  OVERFLOW_POLICIES,
  PEER_IDENTITY_DOMAINS,
  OPERATION_TERMINAL_KINDS,
  SEMANTIC_MAP,
  STREAM_DEFAULTS,
  TRANSITION_TABLES,
  VALID_FIXTURES,
  INVALID_FIXTURES,
  assertBytesWithinLimit,
  createAttachmentTuple,
  createGattPath,
  createPeerIdentity,
  effectiveMaxBytes,
  makeCapabilityDescriptor,
  parseI64Decimal,
  parseU64Decimal,
  validateScanRequest,
  validateStreamLimits,
} from '../src/index';

const ATTACHMENT = {
  attachmentId: 'attach-01',
  backendInstanceId: 'backend-01',
  backendGeneration: 'bg-3',
  adapterId: 'adapter-01',
  adapterGeneration: 'ag-2',
};

// Helpers that preserve JS-callable shapes without casts: `any` parameters
// accept every runtime value, so unknown axes, non-string filter entries, and
// future contender kinds reach validation exactly as a JS caller would send them.
function makeSpanViaJs(axis: any, minimum: any, maximum: any): unknown {
  const mod: any = require('../src/index');
  return mod.makeVersionSpan(axis, minimum, maximum);
}

function scanViaJs(input: any): unknown {
  return validateScanRequest(input);
}

function arbitrateViaJs(operationId: string, dispatched: boolean, kind: any): any {
  const mod: any = require('../src/index');
  return mod.arbitrateContenders({
    operationId,
    dispatched,
    contenders: [{ ingressOrdinal: 1, kind, valid: true }],
  });
}

function genericDeclViaJs(declaration: any): void {
  const mod: any = require('../src/index');
  mod.assertGenericPeripheralDecl(declaration);
}

function tryPushViaJs(table: any, row: any): boolean {
  try {
    table.push(row);
    return true;
  } catch {
    return false;
  }
}

function tryPopViaJs(table: any): void {
  try {
    table.pop();
  } catch {
    // frozen pop throws; nothing to restore
  }
}

function setKeyViaJs(target: any, key: string, value: any): void {
  target[key] = value;
}

function readKeyViaJs(target: any, key: string): unknown {
  return target[key];
}

function hasKeyViaJs(target: any, key: string): boolean {
  return key in target;
}

function descriptorInputViaJs(limits: any, state: string, id: string): any {
  return {
    id,
    state,
    limits,
    limitations:
      state === 'supported'
        ? []
        : [{ code: 'c', explanation: 'e', affectedGuarantee: 'g' }],
    evidence: {
      receiptId: 'receipt-1',
      evidenceLevel: 'deterministic',
      implementationVersion: '4.0.28',
      sourceDigest: 'digest-1',
      scenarioIds: ['scan-start'],
    },
  };
}

describe('R1 effectiveMaxBytes ceiling', () => {
  test('a 1MiB backend max clamps to the frozen 524288 ceiling', () => {
    expect(effectiveMaxBytes([1048576])).toBe(524288);
  });

  test('524289 bytes are rejected even against a 1MiB declaration', () => {
    expect(() => assertBytesWithinLimit(524289, [1048576], 'write')).toThrow('bytes.too-large');
  });
});

describe('R2 frozen tables', () => {
  test('every shared table is frozen', () => {
    const tables: readonly unknown[] = [
      TRANSITION_TABLES,
      ERROR_CODE_LIST,
      ERROR_DOMAIN_LIST,
      BUILT_IN_CAPABILITY_IDS,
      CAPABILITY_STATES,
      CENTRAL_CONTROLS,
      STREAM_DEFAULTS,
      HAPPENS_BEFORE,
      CONTENTION_RULINGS,
      EARLY_EXIT_CLEANUP,
      SEMANTIC_MAP,
      APPROVED_CORRECTIONS,
      VALID_FIXTURES,
      INVALID_FIXTURES,
      PEER_IDENTITY_DOMAINS,
      OVERFLOW_POLICIES,
      OPERATION_TERMINAL_KINDS,
      MANDATORY_SCENARIOS,
    ];
    for (const table of tables) {
      expect(Object.isFrozen(table)).toBe(true);
    }
  });

  test('nested table contents are frozen', () => {
    for (const table of TRANSITION_TABLES) {
      expect(Object.isFrozen(table.states)).toBe(true);
      expect(Object.isFrozen(table.transitions)).toBe(true);
      expect(Object.isFrozen(table.terminals)).toBe(true);
    }
    for (const entry of SEMANTIC_MAP) {
      expect(Object.isFrozen(entry)).toBe(true);
    }
    for (const entry of STREAM_DEFAULTS) {
      expect(Object.isFrozen(entry)).toBe(true);
    }
  });

  test('mutating a frozen table cannot change query results', () => {
    const before = TRANSITION_TABLES.length;
    const pushed = tryPushViaJs(TRANSITION_TABLES, {
      machine: 'manager',
      states: ['ready'],
      transitions: [],
      terminals: ['ready'],
    });
    if (pushed) {
      tryPopViaJs(TRANSITION_TABLES);
    }
    expect(pushed).toBe(false);
    expect(TRANSITION_TABLES.length).toBe(before);
  });
});

describe('R3 capability limits copy-and-freeze', () => {
  test('mutating the source limits after construction leaves the descriptor unchanged', () => {
    const limits: { [key: string]: number } = { maxMtu: 517 };
    const descriptor = makeCapabilityDescriptor(
      descriptorInputViaJs(limits, 'limited', 'connection.request-mtu'),
    );
    setKeyViaJs(limits, 'maxMtu', 999);
    setKeyViaJs(limits, 'extra', 1);
    expect(readKeyViaJs(descriptor.limits, 'maxMtu')).toBe(517);
    expect(hasKeyViaJs(descriptor.limits, 'extra')).toBe(false);
    expect(Object.isFrozen(descriptor.limits)).toBe(true);
  });
});

describe('R4 unknown version axes', () => {
  test('unknown axis spans throw protocol.malformed', () => {
    expect(() => makeSpanViaJs('future-axis', 1, 2)).toThrow('protocol.malformed');
  });
});

describe('R5 unsupported filter fields', () => {
  test('non-string entries throw capability.unsupported', () => {
    expect(() =>
      scanViaJs({
        serviceUuids: [],
        duplicatePolicy: 'all',
        mergePolicy: 'none',
        timeoutMs: 1000,
        hasAbortSignal: false,
        unsupportedFilterFields: [42],
      }),
    ).toThrow('capability.unsupported');
    expect(() =>
      scanViaJs({
        serviceUuids: [],
        duplicatePolicy: 'all',
        mergePolicy: 'none',
        timeoutMs: 1000,
        hasAbortSignal: false,
        unsupportedFilterFields: [{}],
      }),
    ).toThrow('capability.unsupported');
  });

  test('empty-string entries throw capability.unsupported', () => {
    expect(() =>
      scanViaJs({
        serviceUuids: [],
        duplicatePolicy: 'all',
        mergePolicy: 'none',
        timeoutMs: 1000,
        hasAbortSignal: false,
        unsupportedFilterFields: [''],
      }),
    ).toThrow('capability.unsupported');
  });
});

describe('R8 generic-shape allowlist', () => {
  test('unlisted physiological keys are rejected', () => {
    expect(() => genericDeclViaJs({ spo2Sample: [1] })).toThrow('argument.invalid');
    expect(() => genericDeclViaJs({ sleepStage: 'n2' })).toThrow('argument.invalid');
    expect(() => genericDeclViaJs({ glucoseMgDl: 100 })).toThrow('argument.invalid');
  });

  test('listed generic keys are accepted', () => {
    expect(() => genericDeclViaJs({ octetPayload: [1, 2, 3] })).not.toThrow();
  });
});

describe('R9 GATT path invariants', () => {
  test('characteristic uuid without occurrence throws', () => {
    const attachment = createAttachmentTuple(ATTACHMENT);
    const peer = createPeerIdentity(attachment, 'public-address', 'AA:BB:CC:DD:EE:FF');
    expect(() =>
      createGattPath({
        attachment,
        peer,
        connectionGeneration: 'cg-1',
        databaseGeneration: 'dg-1',
        serviceUuid: '180D',
        serviceOccurrence: 0,
        characteristicUuid: '2A37',
        ownerLease: 'lease-1',
      }),
    ).toThrow('argument.invalid');
  });

  test('descriptor without characteristic throws', () => {
    const attachment = createAttachmentTuple(ATTACHMENT);
    const peer = createPeerIdentity(attachment, 'public-address', 'AA:BB:CC:DD:EE:FF');
    expect(() =>
      createGattPath({
        attachment,
        peer,
        connectionGeneration: 'cg-1',
        databaseGeneration: 'dg-1',
        serviceUuid: '180D',
        serviceOccurrence: 0,
        descriptorUuid: '2902',
        descriptorOccurrence: 0,
        ownerLease: 'lease-1',
      }),
    ).toThrow('argument.invalid');
  });

  test('mismatched attachment/peer scope throws', () => {
    const attachment = createAttachmentTuple(ATTACHMENT);
    const foreignAttachment = createAttachmentTuple({ ...ATTACHMENT, backendInstanceId: 'backend-02' });
    const foreignPeer = createPeerIdentity(foreignAttachment, 'public-address', 'AA:BB:CC:DD:EE:FF');
    expect(() =>
      createGattPath({
        attachment,
        peer: foreignPeer,
        connectionGeneration: 'cg-1',
        databaseGeneration: 'dg-1',
        serviceUuid: '180D',
        serviceOccurrence: 0,
        ownerLease: 'lease-1',
      }),
    ).toThrow('peer.scope-mismatch');
  });
});

describe('R11 failure contender and Failed terminal', () => {
  test('a failure contender settles failed', () => {
    const completion: any = arbitrateViaJs('op-fail', true, 'failure');
    expect(completion.terminal).toBe('failed');
  });
});

describe('R12 reserved-control like-with-like', () => {
  test('stream defaults carry reserved-control bytes and item counts', () => {
    const mod: any = require('../src/index');
    for (const entry of mod.STREAM_DEFAULTS) {
      expect(hasKeyViaJs(entry, 'reservedControlCapacity')).toBe(true);
      expect(hasKeyViaJs(entry, 'reservedControlBytes')).toBe(true);
    }
    expect(readKeyViaJs(mod, 'RESERVED_CONTROL_BYTES')).toBeDefined();
  });

  test('byte capacity is compared against reserved bytes, not items', () => {
    const mod: any = require('../src/index');
    expect(() =>
      mod.validateStreamLimits({
        itemCapacity: 64,
        byteCapacity: 64,
        reservedControlCapacity: 1,
        reservedControlBytes: 64,
      }),
    ).toThrow('stream.quota');
    expect(validateStreamLimits).toBeDefined();
  });
});

describe('R14 canonical decimal form', () => {
  test('canonical digit cap is frozen at 20', () => {
    const mod: any = require('../src/index');
    expect(readKeyViaJs(mod, 'MAX_DECIMAL_DIGITS')).toBe(20);
  });

  test('plus sign and whitespace are rejected', () => {
    expect(() => parseU64Decimal('+1')).toThrow('bytes.invalid');
    expect(() => parseU64Decimal(' 1')).toThrow('bytes.invalid');
    expect(() => parseI64Decimal('+1')).toThrow('bytes.invalid');
    expect(() => parseI64Decimal(' 1')).toThrow('bytes.invalid');
  });

  test('digit strings beyond the ~20 digit cap are rejected', () => {
    expect(() => parseU64Decimal('1'.repeat(21))).toThrow('bytes.invalid');
    expect(() => parseI64Decimal(`-${'9'.repeat(21)}`)).toThrow('bytes.invalid');
  });
});
