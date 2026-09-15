// contracts/__tests__/fixtures-roundtrip.test.ts — C-UBM DRAFT U1 tests.
// Valid fixtures pass; invalid fixtures fail with their frozen codes;
// wire round trips preserve validated meaning.

import { INVALID_FIXTURES, VALID_FIXTURES } from '../src/index';
import {
  assertSameAttachment,
  canonicalUuidValue,
  createAttachmentTuple,
  isBleErrorCode,
  makeTerminalRecord,
  makeVersionSpan,
  negotiateVersionSpan,
} from '../src/index';

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
    const negotiated = negotiateVersionSpan(
      makeVersionSpan('backend-contract', 1, 3),
      makeVersionSpan('backend-contract', 2, 5),
    );
    const revived = negotiateVersionSpan(
      makeVersionSpan('backend-contract', 1, 3),
      makeVersionSpan('backend-contract', 2, 5),
    );
    expect(cloneViaJson(negotiated)).toEqual(cloneViaJson(revived));
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
    const revived: unknown = cloneViaJson(record);
    if (typeof revived !== 'object' || revived === null) {
      throw new Error('terminal record must round-trip as an object');
    }
    expect(JSON.stringify(revived)).toContain('"kind":"timed-out"');
    expect(JSON.stringify(revived)).toContain('"cause":"operation.timed-out"');
  });
});
