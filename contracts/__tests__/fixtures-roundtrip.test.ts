// contracts/__tests__/fixtures-roundtrip.test.ts — C-UBM DRAFT U1 tests.
// Valid fixtures pass; invalid fixtures fail with their frozen codes;
// wire round trips preserve validated meaning.
// No casts: fixture values flow through `any`-typed accessors so unknown
// shapes reach validators exactly as a JS caller would send them.

import { INVALID_FIXTURES, VALID_FIXTURES } from '../src/index';
import {
  assertSameAttachment,
  assertTimeoutMs,
  canonicalBleAddressValue,
  canonicalUuidValue,
  createAttachmentTuple,
  createGattPath,
  createPeerIdentity,
  isBleErrorCode,
  makeTerminalRecord,
  parseI64Decimal,
  parseU64Decimal,
  assertBytesWithinLimit,
  toDeadline,
  MAX_OPERATION_BYTES,
} from '../src/index';
import { assertNegotiatedWithinOffer } from '../src/index';

function cloneViaJson(value: unknown): unknown {
  return JSON.parse(JSON.stringify(value));
}

function wireField(record: object, key: string): unknown {
  for (const [entryKey, entryValue] of Object.entries(record)) {
    if (entryKey === key) {
      return entryValue;
    }
  }
  return undefined;
}

function spanViaJs(axis: any, minimum: any, maximum: any): any {
  const mod: any = require('../src/index');
  return mod.makeVersionSpan(axis, minimum, maximum);
}

function negotiateViaJs(local: any, remote: any): any {
  const mod: any = require('../src/index');
  return mod.negotiateVersionSpan(local, remote);
}

const BASE_ATTACHMENT_VALUE = {
  attachmentId: 'attach-01',
  backendInstanceId: 'backend-01',
  backendGeneration: 'bg-3',
  adapterId: 'adapter-01',
  adapterGeneration: 'ag-2',
};

function executeValidFixture(fixture: { readonly name: string; readonly kind: string; readonly value: unknown }): void {
  const value: any = fixture.value;
  switch (fixture.kind) {
    case 'attachment': {
      createAttachmentTuple({
        attachmentId: value.attachmentId,
        backendInstanceId: value.backendInstanceId,
        backendGeneration: value.backendGeneration,
        adapterId: value.adapterId,
        adapterGeneration: value.adapterGeneration,
      });
      return;
    }
    case 'peer': {
      const attachment = createAttachmentTuple(BASE_ATTACHMENT_VALUE);
      createPeerIdentity(attachment, value.domain, value.value);
      return;
    }
    case 'scan-request': {
      const mod: any = require('../src/index');
      mod.validateScanRequest({
        serviceUuids: value.serviceUuids,
        duplicatePolicy: value.duplicatePolicy,
        mergePolicy: value.mergePolicy,
        timeoutMs: value.timeoutMs,
        hasAbortSignal: value.hasAbortSignal,
      });
      return;
    }
    case 'gatt-path': {
      const attachment = createAttachmentTuple(BASE_ATTACHMENT_VALUE);
      const peer = createPeerIdentity(attachment, 'public-address', 'AA:BB:CC:DD:EE:FF');
      createGattPath({
        attachment,
        peer,
        connectionGeneration: value.connectionGeneration,
        databaseGeneration: value.databaseGeneration,
        serviceUuid: value.serviceUuid,
        serviceOccurrence: value.serviceOccurrence,
        characteristicUuid: value.characteristicUuid,
        characteristicOccurrence: value.characteristicOccurrence,
        descriptorUuid: value.descriptorUuid,
        descriptorOccurrence: value.descriptorOccurrence,
        ownerLease: value.ownerLease,
      });
      return;
    }
    case 'stream-limits': {
      const mod: any = require('../src/index');
      mod.validateStreamLimits({
        itemCapacity: value.itemCapacity,
        byteCapacity: value.byteCapacity,
        reservedControlCapacity: value.reservedControlCapacity,
        reservedControlBytes: value.reservedControlBytes,
      });
      return;
    }
    case 'version-span': {
      spanViaJs(value.axis, value.minimum, value.maximum);
      return;
    }
    case 'deadline': {
      toDeadline(value.nowMs, value.timeoutMs);
      return;
    }
    case 'capability': {
      const mod: any = require('../src/index');
      mod.makeCapabilityDescriptor(value);
      return;
    }
    case 'ownership-transfer': {
      const mod: any = require('../src/index');
      mod.validateOwnershipTransfer({
        resourceKind: value.resourceKind,
        sourceClient: value.sourceClient,
        destinationClient: value.destinationClient,
        generation: value.generation,
        transferEpoch: value.transferEpoch,
      });
      return;
    }
    case 'peripheral-service': {
      const mod: any = require('../src/index');
      const characteristics: any = value.characteristics ?? [];
      mod.validateServiceDecl({
        uuid: value.uuid,
        primary: value.primary,
        characteristics,
      });
      return;
    }
    case 'uuid': {
      canonicalUuidValue(value);
      return;
    }
    case 'ble-address': {
      canonicalBleAddressValue(value);
      return;
    }
    case 'timeout': {
      assertTimeoutMs(value, 'fixture.timeout');
      return;
    }
    case 'u64': {
      parseU64Decimal(value);
      return;
    }
    case 'i64': {
      parseI64Decimal(value);
      return;
    }
    case 'write-length': {
      const mod: any = require('../src/index');
      mod.assertBytesWithinLimit(value, [MAX_OPERATION_BYTES], 'fixture.write-length');
      if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < 0) {
        throw new Error(`fixture ${fixture.name} write length must be a safe non-negative integer`);
      }
      assertBytesWithinLimit(value, [MAX_OPERATION_BYTES], 'fixture.write-length');
      return;
    }
    default:
      throw new Error(`fixture ${fixture.name} has unknown kind ${fixture.kind}`);
  }
}

describe('valid fixtures', () => {
  test('every valid fixture carries a name, kind, and value', () => {
    expect(VALID_FIXTURES.length > 0).toBe(true);
    for (const fixture of VALID_FIXTURES) {
      expect(fixture.name.length > 0).toBe(true);
      expect(fixture.kind.length > 0).toBe(true);
    }
  });

  test('includes 64-bit boundary values', () => {
    const names = VALID_FIXTURES.map(fixture => fixture.name);
    expect(names.includes('u64-max')).toBe(true);
    expect(names.includes('u64-zero')).toBe(true);
    expect(names.includes('i64-min')).toBe(true);
    expect(names.includes('i64-max')).toBe(true);
  });

  test('every valid fixture executes through its validator by kind', () => {
    expect(VALID_FIXTURES.length > 0).toBe(true);
    for (const fixture of VALID_FIXTURES) {
      try {
        executeValidFixture(fixture);
      } catch (error) {
        throw new Error(
          `valid fixture ${fixture.name} (${fixture.kind}) must execute: ${error instanceof Error ? error.message : 'unknown'}`,
        );
      }
    }
  });
});

describe('invalid fixtures', () => {
  test('every invalid fixture fails with its frozen code', () => {
    expect(INVALID_FIXTURES.length > 0).toBe(true);
    for (const fixture of INVALID_FIXTURES) {
      expect(isBleErrorCode(fixture.expectedCode)).toBe(true);
      let thrown: string | null = null;
      try {
        fixture.check();
      } catch (error) {
        thrown = error instanceof Error ? error.message : 'unknown';
      }
      if (thrown === null || !thrown.includes(fixture.expectedCode)) {
        throw new Error(`fixture ${fixture.name} must throw ${fixture.expectedCode}`);
      }
    }
  });
});

describe('wire round trips', () => {
  test('attachment tuples survive JSON with identical meaning', () => {
    const tuple = createAttachmentTuple({
      attachmentId: 'attach-01',
      backendInstanceId: 'backend-01',
      backendGeneration: 'bg-3',
      adapterId: 'adapter-01',
      adapterGeneration: 'ag-2',
    });
    const wire = cloneViaJson(tuple);
    if (typeof wire !== 'object' || wire === null || Array.isArray(wire)) {
      throw new Error('attachment must round-trip as an object');
    }
    const revived = createAttachmentTuple({
      attachmentId: wireField(wire, 'attachmentId'),
      backendInstanceId: wireField(wire, 'backendInstanceId'),
      backendGeneration: wireField(wire, 'backendGeneration'),
      adapterId: wireField(wire, 'adapterId'),
      adapterGeneration: wireField(wire, 'adapterGeneration'),
    });
    expect(() => assertSameAttachment(revived, tuple, 'read')).not.toThrow();
  });

  test('canonical UUIDs survive JSON unchanged', () => {
    const canonical = canonicalUuidValue('180D');
    const revived: unknown = cloneViaJson(canonical);
    expect(canonicalUuidValue(revived)).toBe(canonical);
  });

  test('negotiated versions survive JSON with identical selection', () => {
    const local = spanViaJs('backend-contract', 1, 3);
    const remote = spanViaJs('backend-contract', 2, 5);
    const negotiated: any = negotiateViaJs(local, remote);
    const wire: unknown = cloneViaJson(negotiated);
    if (typeof wire !== 'object' || wire === null) {
      throw new Error('negotiated version must round-trip as an object');
    }
    expect(wireField(wire, 'selected')).toBe(negotiated.selected);
    expect(wireField(wire, 'axis')).toBe(negotiated.axis);
    expect(wireField(wire, 'localMinimum')).toBe(negotiated.localMinimum);
    expect(wireField(wire, 'remoteMaximum')).toBe(negotiated.remoteMaximum);
    expect(() => assertNegotiatedWithinOffer(negotiated, local)).not.toThrow();
  });

  test('terminal records survive JSON with identical outcome', () => {
    const record = makeTerminalRecord({
      operationId: 'op-1',
      kind: 'timed-out',
      cause: 'operation.timed-out',
      ingressOrdinal: 2,
      startedAt: 100,
      settledAt: 200,
    });
    const wire: unknown = cloneViaJson(record);
    if (typeof wire !== 'object' || wire === null) {
      throw new Error('terminal record must round-trip as an object');
    }
    const revived = makeTerminalRecord({
      operationId: wireField(wire, 'operationId'),
      kind: wireField(wire, 'kind'),
      cause: wireField(wire, 'cause'),
      ingressOrdinal: wireField(wire, 'ingressOrdinal'),
      startedAt: wireField(wire, 'startedAt'),
      settledAt: wireField(wire, 'settledAt'),
    });
    expect(revived).toEqual(record);
  });
});
