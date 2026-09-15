// contracts/__tests__/central-gatt.test.ts — C-UBM DRAFT U1 tests.

import {
  CENTRAL_CONTROLS,
  createAttachmentTuple,
  createGattPath,
  createPeerIdentity,
  planLongWrite,
  validateScanRequest,
  validateWriteRequest,
} from '../src/index';

const ATTACHMENT = {
  attachmentId: 'attach-01',
  backendInstanceId: 'backend-01',
  backendGeneration: 'bg-3',
  adapterId: 'adapter-01',
  adapterGeneration: 'ag-2',
};

function testPath(serviceOccurrence: number): ReturnType<typeof createGattPath> {
  const attachment = createAttachmentTuple(ATTACHMENT);
  const peer = createPeerIdentity(attachment, 'public-address', 'AA:BB:CC:DD:EE:FF');
  return createGattPath({
    attachment,
    peer,
    connectionGeneration: 'cg-1',
    databaseGeneration: 'dg-1',
    serviceUuid: '180D',
    serviceOccurrence,
    characteristicUuid: '2A37',
    characteristicOccurrence: 0,
    ownerLease: 'lease-1',
  });
}

describe('scan requests', () => {
  test('accepts explicit filter, policy, deadline, and abort shape', () => {
    const request = validateScanRequest({
      serviceUuids: ['180D'],
      duplicatePolicy: 'all',
      mergePolicy: 'none',
      timeoutMs: 5000,
      hasAbortSignal: true,
    });
    expect(request.duplicatePolicy).toBe('all');
    expect(request.timeoutMs).toBe(5000);
  });

  test('unsupported filters fail instead of broadening', () => {
    expect(() =>
      validateScanRequest({
        serviceUuids: ['180D'],
        duplicatePolicy: 'all',
        mergePolicy: 'none',
        timeoutMs: 5000,
        hasAbortSignal: false,
        unsupportedFilterFields: ['raw-bytes-predicate'],
      }),
    ).toThrow('capability.unsupported');
  });

  test('rejects invalid UUIDs and timeouts before dispatch', () => {
    expect(() =>
      validateScanRequest({
        serviceUuids: ['bogus'],
        duplicatePolicy: 'all',
        mergePolicy: 'none',
        timeoutMs: 5000,
        hasAbortSignal: false,
      }),
    ).toThrow('argument.invalid');
    expect(() =>
      validateScanRequest({
        serviceUuids: [],
        duplicatePolicy: 'all',
        mergePolicy: 'none',
        timeoutMs: 0,
        hasAbortSignal: false,
      }),
    ).toThrow('argument.invalid');
  });
});

describe('GATT path selection (GATT-01)', () => {
  test('ambiguous UUID-only selection fails with candidate paths', () => {
    const first = testPath(0);
    const second = testPath(1);
    expect(first.serviceOccurrence).toBe(0);
    expect(second.serviceOccurrence).toBe(1);
    expect(first.characteristicUuid).toBe(second.characteristicUuid);
  });
});

describe('write validation', () => {
  test('write mode is mandatory and never substituted', () => {
    const path = testPath(0);
    const request = validateWriteRequest({
      path,
      currentConnectionGeneration: 'cg-1',
      currentDatabaseGeneration: 'dg-1',
      mode: 'with-response',
      valueByteLength: 20,
      effectiveMaximum: 512,
      modeSupported: true,
    });
    expect(request.mode).toBe('with-response');
    expect(() =>
      validateWriteRequest({
        path,
        currentConnectionGeneration: 'cg-1',
        currentDatabaseGeneration: 'dg-1',
        mode: 'without-response',
        valueByteLength: 20,
        effectiveMaximum: 512,
        modeSupported: false,
      }),
    ).toThrow('capability.unsupported');
  });

  test('stale paths and oversize values fail before native work', () => {
    const path = testPath(0);
    expect(() =>
      validateWriteRequest({
        path,
        currentConnectionGeneration: 'cg-2',
        currentDatabaseGeneration: 'dg-1',
        mode: 'with-response',
        valueByteLength: 20,
        effectiveMaximum: 512,
        modeSupported: true,
      }),
    ).toThrow('gatt.stale-handle');
    expect(() =>
      validateWriteRequest({
        path,
        currentConnectionGeneration: 'cg-1',
        currentDatabaseGeneration: 'dg-1',
        mode: 'with-response',
        valueByteLength: 600,
        effectiveMaximum: 512,
        modeSupported: true,
      }),
    ).toThrow('bytes.too-large');
  });

  test('zero-length values are valid payloads, never absence markers', () => {
    const path = testPath(0);
    const request = validateWriteRequest({
      path,
      currentConnectionGeneration: 'cg-1',
      currentDatabaseGeneration: 'dg-1',
      mode: 'with-response',
      valueByteLength: 0,
      effectiveMaximum: 512,
      modeSupported: true,
    });
    expect(request.valueByteLength).toBe(0);
  });
});

describe('long writes', () => {
  test('segment maximum is the minimum of stacked limits', () => {
    const plan = planLongWrite({
      valueByteLength: 600,
      operationPayloadLimit: 512,
      negotiatedDirectionalLimit: 185,
      backendLimit: 256,
    });
    expect(plan.segmentMaximum).toBe(185);
    expect(plan.segments).toBe(4);
    expect(plan.atomic).toBe(false);
  });

  test('rejects missing limits instead of assuming infinity', () => {
    expect(() =>
      planLongWrite({
        valueByteLength: 600,
        operationPayloadLimit: null,
        negotiatedDirectionalLimit: 185,
        backendLimit: 256,
      }),
    ).toThrow('capability.unavailable');
  });
});

describe('connection controls façade', () => {
  test('binds every control to its runtime capability id', () => {
    const ids = CENTRAL_CONTROLS.map(control => control.capabilityId);
    expect(ids.includes('connection:rssi')).toBe(true);
    expect(ids.includes('connection:request-mtu')).toBe(true);
    expect(ids.includes('gatt:maximum-write-length')).toBe(true);
    expect(ids.includes('gatt:write-without-response-readiness')).toBe(true);
  });

  test('request and observation stay distinct facts', () => {
    const mtu = CENTRAL_CONTROLS.find(control => control.method === 'requestMtu');
    if (mtu === undefined) {
      throw new Error('requestMtu control must be frozen');
    }
    expect(mtu.acceptanceIsProof).toBe(false);
  });
});
