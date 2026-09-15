// contracts/__tests__/capabilities.test.ts — C-UBM DRAFT U1 tests.

import {
  BUILT_IN_CAPABILITY_IDS,
  assertCapabilityAllows,
  isBuiltInCapabilityId,
  makeCapabilityDescriptor,
} from '../src/index';

describe('four-state capability vocabulary', () => {
  test('supported capabilities proceed', () => {
    const descriptor = makeCapabilityDescriptor({
      id: 'central.scan',
      state: 'supported',
      limits: {},
      limitations: [],
      evidence: {
        receiptId: 'receipt-1',
        evidenceLevel: 'deterministic',
        implementationVersion: '4.0.28',
        sourceDigest: 'digest-1',
        scenarioIds: ['scan-start'],
      },
    });
    expect(assertCapabilityAllows(descriptor, 'scan.start')).toBe('proceed');
  });

  test('unsupported and unavailable capabilities fail with typed codes', () => {
    const unsupported = makeCapabilityDescriptor({
      id: 'connection.priority',
      state: 'unsupported',
      limits: {},
      limitations: [{ code: 'bluez.no-parameter-surface', explanation: 'no surface', affectedGuarantee: 'priority' }],
      evidence: {
        receiptId: 'receipt-2',
        evidenceLevel: 'blocked',
        implementationVersion: '4.0.28',
        sourceDigest: 'digest-2',
        scenarioIds: [],
      },
    });
    expect(() => assertCapabilityAllows(unsupported, 'connection.requestPriority')).toThrow(
      'capability.unsupported',
    );
    const unavailable = makeCapabilityDescriptor({
      id: 'connection.rssi',
      state: 'unavailable',
      limits: {},
      limitations: [{ code: 'no-measurement', explanation: 'not measured', affectedGuarantee: 'rssi' }],
      evidence: {
        receiptId: 'receipt-3',
        evidenceLevel: 'blocked',
        implementationVersion: '4.0.28',
        sourceDigest: 'digest-3',
        scenarioIds: [],
      },
    });
    expect(() => assertCapabilityAllows(unavailable, 'connection.readRssi')).toThrow(
      'capability.unavailable',
    );
  });

  test('limited capabilities proceed only with their named limitation', () => {
    const limited = makeCapabilityDescriptor({
      id: 'connection.request-mtu',
      state: 'limited',
      limits: { maxMtu: 517 },
      limitations: [
        { code: 'callback-derived', explanation: 'observation only', affectedGuarantee: 'negotiation' },
      ],
      evidence: {
        receiptId: 'receipt-4',
        evidenceLevel: 'deterministic',
        implementationVersion: '4.0.28',
        sourceDigest: 'digest-4',
        scenarioIds: ['mtu-request'],
      },
    });
    expect(assertCapabilityAllows(limited, 'connection.requestMtu')).toBe(
      'proceed-with-limitation',
    );
  });

  test('non-supported states require a reason', () => {
    expect(() =>
      makeCapabilityDescriptor({
        id: 'x',
        state: 'unsupported',
        limits: {},
        limitations: [],
        evidence: {
          receiptId: 'r',
          evidenceLevel: 'blocked',
          implementationVersion: '4.0.28',
          sourceDigest: 'd',
          scenarioIds: [],
        },
      }),
    ).toThrow('argument.invalid');
  });
});

describe('built-in capability catalog', () => {
  test('covers the connection-controls surface', () => {
    expect(isBuiltInCapabilityId('connection:rssi')).toBe(true);
    expect(isBuiltInCapabilityId('connection:request-mtu')).toBe(true);
    expect(isBuiltInCapabilityId('gatt:write-without-response-readiness')).toBe(true);
    expect(isBuiltInCapabilityId('central.scan')).toBe(true);
    expect(isBuiltInCapabilityId('peripheral.advertise')).toBe(true);
    expect(isBuiltInCapabilityId('nope.unknown')).toBe(false);
  });

  test('every catalog id is guarded consistently', () => {
    for (const id of BUILT_IN_CAPABILITY_IDS) {
      expect(isBuiltInCapabilityId(id)).toBe(true);
    }
  });
});
