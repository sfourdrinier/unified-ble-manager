// contracts/__tests__/identities-handles.test.ts — C-UBM DRAFT U1 tests.

import {
  attachmentTuplesEqual,
  assertCurrentGeneration,
  assertSameAttachment,
  canonicalBleAddressValue,
  canonicalUuidValue,
  createAttachmentTuple,
  createGattPath,
  createHandleRef,
  createPeerIdentity,
  isGenerationCurrent,
  isGloballyStableDomain,
  isNonEmptyId,
  peerSessionKey,
} from '../src/index';

const ATTACHMENT = {
  attachmentId: 'attach-01',
  backendInstanceId: 'backend-01',
  backendGeneration: 'bg-3',
  adapterId: 'adapter-01',
  adapterGeneration: 'ag-2',
};

describe('opaque identifiers', () => {
  test('rejects empty or non-string ids', () => {
    expect(isNonEmptyId('peer-1')).toBe(true);
    expect(isNonEmptyId('')).toBe(false);
    expect(isNonEmptyId(42)).toBe(false);
    expect(isNonEmptyId(null)).toBe(false);
    expect(isNonEmptyId(undefined)).toBe(false);
  });

  test('attachment tuples require every field', () => {
    const tuple = createAttachmentTuple(ATTACHMENT);
    expect(tuple.attachmentId).toBe('attach-01');
    expect(() =>
      createAttachmentTuple({ ...ATTACHMENT, adapterId: '' }),
    ).toThrow('argument.invalid');
    expect(() => createAttachmentTuple({ ...ATTACHMENT, backendGeneration: 7 })).toThrow(
      'argument.invalid',
    );
  });

  test('attachment equality compares the complete tuple', () => {
    const left = createAttachmentTuple(ATTACHMENT);
    const right = createAttachmentTuple(ATTACHMENT);
    expect(attachmentTuplesEqual(left, right)).toBe(true);
    const rotated = createAttachmentTuple({ ...ATTACHMENT, backendInstanceId: 'backend-02' });
    expect(attachmentTuplesEqual(left, rotated)).toBe(false);
  });
});

describe('stale handles fail before dispatch (OWN-02)', () => {
  test('same attachment passes', () => {
    const current = createAttachmentTuple(ATTACHMENT);
    expect(() => assertSameAttachment(current, current, 'read')).not.toThrow();
  });

  test('rotated backend instance rejects even when visible ids repeat', () => {
    const current = createAttachmentTuple(ATTACHMENT);
    const stale = createAttachmentTuple({ ...ATTACHMENT, backendInstanceId: 'backend-02' });
    expect(() => assertSameAttachment(stale, current, 'read')).toThrow('connection.stale');
  });

  test('rotated backend generation rejects', () => {
    const current = createAttachmentTuple(ATTACHMENT);
    const stale = createAttachmentTuple({ ...ATTACHMENT, backendGeneration: 'bg-2' });
    expect(() => assertSameAttachment(stale, current, 'write')).toThrow('connection.stale');
  });
});

describe('peer identities', () => {
  test('creates session-scoped peer keys', () => {
    const attachment = createAttachmentTuple(ATTACHMENT);
    const peer = createPeerIdentity(attachment, 'public-address', 'AA:BB:CC:DD:EE:FF');
    expect(peerSessionKey(peer)).toBe('public-address:AA:BB:CC:DD:EE:FF');
  });

  test('rejects empty domains and values', () => {
    const attachment = createAttachmentTuple(ATTACHMENT);
    expect(() => createPeerIdentity(attachment, '', 'value')).toThrow('argument.invalid');
    expect(() => createPeerIdentity(attachment, 'opaque-token', '')).toThrow('argument.invalid');
  });

  test('address-like rotating domains are never globally stable', () => {
    expect(isGloballyStableDomain('public-address')).toBe(true);
    expect(isGloballyStableDomain('static-random-address')).toBe(true);
    expect(isGloballyStableDomain('resolvable-private-address')).toBe(false);
    expect(isGloballyStableDomain('opaque-token')).toBe(false);
    expect(isGloballyStableDomain('platform-guid')).toBe(false);
  });
});

describe('UUID canonicalization', () => {
  test('expands 16-bit Bluetooth-base UUIDs', () => {
    expect(canonicalUuidValue('180D')).toBe('0000180d-0000-1000-8000-00805f9b34fb');
  });

  test('expands 32-bit UUIDs and lowercases 128-bit UUIDs', () => {
    expect(canonicalUuidValue('12345678')).toBe('12345678-0000-1000-8000-00805f9b34fb');
    expect(canonicalUuidValue('0000180D-0000-1000-8000-00805F9B34FB')).toBe(
      '0000180d-0000-1000-8000-00805f9b34fb',
    );
  });

  test('rejects invalid UUID input before discovery', () => {
    expect(() => canonicalUuidValue('not-a-uuid')).toThrow('argument.invalid');
    expect(() => canonicalUuidValue('123')).toThrow('argument.invalid');
    expect(() => canonicalUuidValue('')).toThrow('argument.invalid');
    expect(() => canonicalUuidValue(1234)).toThrow('argument.invalid');
  });
});

describe('BLE address canonicalization', () => {
  test('normalizes separators and case', () => {
    expect(canonicalBleAddressValue('aa-bb-cc-dd-ee-ff')).toBe('AA:BB:CC:DD:EE:FF');
    expect(canonicalBleAddressValue('AA:BB:CC:DD:EE:FF')).toBe('AA:BB:CC:DD:EE:FF');
  });

  test('rejects malformed addresses', () => {
    expect(() => canonicalBleAddressValue('AA:BB:CC:DD:EE')).toThrow('argument.invalid');
    expect(() => canonicalBleAddressValue('not-an-address')).toThrow('argument.invalid');
    expect(() => canonicalBleAddressValue('')).toThrow('argument.invalid');
  });
});

describe('GATT paths use occurrence identity, never UUID-first lookup', () => {
  test('two equal UUIDs at different occurrences are distinct paths', () => {
    const attachment = createAttachmentTuple(ATTACHMENT);
    const peer = createPeerIdentity(attachment, 'public-address', 'AA:BB:CC:DD:EE:FF');
    const first = createGattPath({
      attachment,
      peer,
      connectionGeneration: 'cg-1',
      databaseGeneration: 'dg-1',
      serviceUuid: '180D',
      serviceOccurrence: 0,
      characteristicUuid: '2A37',
      characteristicOccurrence: 0,
      ownerLease: 'lease-1',
    });
    const second = createGattPath({
      attachment,
      peer,
      connectionGeneration: 'cg-1',
      databaseGeneration: 'dg-1',
      serviceUuid: '180D',
      serviceOccurrence: 1,
      characteristicUuid: '2A37',
      characteristicOccurrence: 0,
      ownerLease: 'lease-1',
    });
    expect(first.serviceOccurrence).toBe(0);
    expect(second.serviceOccurrence).toBe(1);
    expect(first).not.toEqual(second);
  });

  test('handles bind attachment, generations, path, and owner lease', () => {
    const attachment = createAttachmentTuple(ATTACHMENT);
    const peer = createPeerIdentity(attachment, 'public-address', 'AA:BB:CC:DD:EE:FF');
    const path = createGattPath({
      attachment,
      peer,
      connectionGeneration: 'cg-1',
      databaseGeneration: 'dg-1',
      serviceUuid: '180D',
      serviceOccurrence: 0,
      characteristicUuid: '2A37',
      characteristicOccurrence: 0,
      ownerLease: 'lease-1',
    });
    const handle = createHandleRef(path, 'lease-1');
    expect(handle.ownerLease).toBe('lease-1');
    expect(handle.connectionGeneration).toBe('cg-1');
  });
});

describe('generation currency', () => {
  test('only the current generation is usable', () => {
    expect(isGenerationCurrent('cg-1', 'cg-1')).toBe(true);
    expect(isGenerationCurrent('cg-1', 'cg-2')).toBe(false);
    expect(() => assertCurrentGeneration('cg-1', 'cg-2', 'read')).toThrow('connection.stale');
    expect(() => assertCurrentGeneration('cg-2', 'cg-2', 'read')).not.toThrow();
  });
});
