// contracts/__tests__/peripheral-roles.test.ts — C-UBM DRAFT U1 tests.
// Generic peripheral extension primitives: no commercial or physiological content.

import {
  PERIPHERAL_FORBIDDEN_KEY_SUBSTRINGS,
  arbitrateServerResponse,
  assertAdvertisementWithinLimits,
  assertAtomicCommit,
  assertGenericPeripheralDecl,
  reconcileCccdOnUnsubscribe,
  validateServiceDecl,
} from '../src/index';

describe('generic peripheral declarations', () => {
  test('accepts service/characteristic/descriptor declarations', () => {
    const service = validateServiceDecl({
      uuid: '180D',
      primary: true,
      characteristics: [
        {
          uuid: '2A37',
          properties: ['notify'],
          descriptors: [{ uuid: '2902' }],
        },
      ],
    });
    expect(service.primary).toBe(true);
    expect(service.characteristics.length).toBe(1);
  });

  test('rejects invalid UUIDs and empty characteristics', () => {
    expect(() =>
      validateServiceDecl({ uuid: 'bogus', primary: true, characteristics: [] }),
    ).toThrow('argument.invalid');
  });

  test('rejects commercial or physiological fields', () => {
    for (const key of ['ecgSample', 'paymentSku', 'heartRate']) {
      expect(() => assertGenericPeripheralDecl({ [key]: [1, 2] })).toThrow('argument.invalid');
    }
    expect(PERIPHERAL_FORBIDDEN_KEY_SUBSTRINGS.includes('ecg')).toBe(true);
    expect(PERIPHERAL_FORBIDDEN_KEY_SUBSTRINGS.includes('payment')).toBe(true);
  });

  test('accepts generic byte payloads', () => {
    expect(() => assertGenericPeripheralDecl({ octetPayload: [1, 2, 3] })).not.toThrow();
  });
});

describe('advertising limits (SRV-07)', () => {
  test('accepts payloads within host limits', () => {
    expect(() =>
      assertAdvertisementWithinLimits({ payloadBytes: 31, hostMaximum: 31, fields: ['flags'] }),
    ).not.toThrow();
  });

  test('rejects oversize payloads explicitly instead of dropping fields', () => {
    expect(() =>
      assertAdvertisementWithinLimits({ payloadBytes: 64, hostMaximum: 31, fields: ['flags'] }),
    ).toThrow('bytes.too-large');
    expect(() => assertAdvertisementWithinLimits({ payloadBytes: 10, hostMaximum: null, fields: [] })).toThrow(
      'capability.unavailable',
    );
  });
});

describe('CCCD reconciliation (SRV-04, GATT-03)', () => {
  test('removing one consumer keeps other peers subscribed', () => {
    const remaining = reconcileCccdOnUnsubscribe({
      bindings: [
        { peerLease: 'lease-a', characteristic: '2A37', enabled: true, kind: 'notification' },
        { peerLease: 'lease-b', characteristic: '2A37', enabled: true, kind: 'notification' },
      ],
      leavingPeerLease: 'lease-a',
      characteristic: '2A37',
    });
    expect(remaining.physicalEnabled).toBe(true);
    expect(remaining.bindings.length).toBe(1);
  });

  test('last consumer disables physical enablement', () => {
    const remaining = reconcileCccdOnUnsubscribe({
      bindings: [{ peerLease: 'lease-a', characteristic: '2A37', enabled: true, kind: 'notification' }],
      leavingPeerLease: 'lease-a',
      characteristic: '2A37',
    });
    expect(remaining.physicalEnabled).toBe(false);
    expect(remaining.bindings.length).toBe(0);
  });
});

describe('atomic transactions (SRV-03)', () => {
  test('accepts fully validated commits', () => {
    expect(() =>
      assertAtomicCommit({
        preparedSegments: 3,
        validatedSegments: 3,
        cancelled: false,
        failedValidation: false,
      }),
    ).not.toThrow();
  });

  test('never reports partial commits as atomic success', () => {
    expect(() =>
      assertAtomicCommit({
        preparedSegments: 3,
        validatedSegments: 2,
        cancelled: false,
        failedValidation: false,
      }),
    ).toThrow('protocol.violation');
    expect(() =>
      assertAtomicCommit({
        preparedSegments: 3,
        validatedSegments: 3,
        cancelled: true,
        failedValidation: false,
      }),
    ).toThrow('operation.aborted');
  });
});

describe('server response deadlines (SRV-06)', () => {
  test('first valid response wins; repeats are stale', () => {
    const first = arbitrateServerResponse({
      requestId: 'req-1',
      responded: false,
      nowMs: 1000,
      deadlineMs: 2000,
    });
    expect(first).toBe('respond');
    const repeat = arbitrateServerResponse({
      requestId: 'req-1',
      responded: true,
      nowMs: 1100,
      deadlineMs: 2000,
    });
    expect(repeat).toBe('stale-ignored');
    const late = arbitrateServerResponse({
      requestId: 'req-1',
      responded: false,
      nowMs: 2500,
      deadlineMs: 2000,
    });
    expect(late).toBe('deadline-expired');
  });
});
