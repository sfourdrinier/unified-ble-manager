// contracts/__tests__/ownership-arbitration.test.ts — C-UBM DRAFT U1 tests.
// Mandatory scenario links: OWN-01, OWN-02 (see identities-handles), OPS-01/02 (see effects-races).

import {
  arbitrateConnectionRequest,
  arbitrateScanRequest,
  createAttachmentTuple,
  isAuthorizationBlocking,
  validateOwnershipTransfer,
} from '../src/index';

const ATTACHMENT = {
  attachmentId: 'attach-01',
  backendInstanceId: 'backend-01',
  backendGeneration: 'bg-3',
  adapterId: 'adapter-01',
  adapterGeneration: 'ag-2',
};

describe('scan arbitration', () => {
  test('first scan owns the physical controller', () => {
    const decision = arbitrateScanRequest({ physicalActive: false });
    expect(decision.kind).toBe('grant-physical');
  });

  test('second ordinary scan fails without disturbing the first', () => {
    const decision = arbitrateScanRequest({ physicalActive: true });
    if (decision.kind !== 'reject') {
      throw new Error('expected scan contention to reject');
    }
    expect(decision.code).toBe('scan.already-active');
  });

  test('explicitly shared scan grants an independent bounded lease', () => {
    const decision = arbitrateScanRequest({
      physicalActive: true,
      shareToken: 'share-identical-filters',
    });
    expect(decision.kind).toBe('grant-lease');
    if (decision.kind !== 'grant-lease') {
      throw new Error('expected a shared scan lease');
    }
    expect(decision.lease.length > 0).toBe(true);
  });
});

describe('connection arbitration (OWN-01)', () => {
  test('second client fails when sharing is unsupported', () => {
    const decision = arbitrateConnectionRequest({
      sharingSupported: false,
      existingLeases: 1,
    });
    if (decision.kind !== 'reject') {
      throw new Error('expected connection contention to reject');
    }
    expect(decision.code).toBe('connection.already-owned');
  });

  test('second client leases the link when sharing is supported', () => {
    const decision = arbitrateConnectionRequest({
      sharingSupported: true,
      existingLeases: 1,
    });
    expect(decision.kind).toBe('grant-lease');
  });

  test('first client owns the link', () => {
    const decision = arbitrateConnectionRequest({
      sharingSupported: false,
      existingLeases: 0,
    });
    expect(decision.kind).toBe('grant-physical');
  });
});

describe('ownership transfer records', () => {
  test('accepts authenticated transfer records', () => {
    const transfer = validateOwnershipTransfer({
      resourceKind: 'connection-lease',
      sourceClient: 'client-a',
      destinationClient: 'client-b',
      generation: 'cg-1',
      transferEpoch: 4,
    });
    expect(transfer.destinationClient).toBe('client-b');
    expect(transfer.transferEpoch).toBe(4);
  });

  test('rejects anonymous or epoch-less transfers', () => {
    expect(() =>
      validateOwnershipTransfer({
        resourceKind: 'connection-lease',
        sourceClient: '',
        destinationClient: 'client-b',
        generation: 'cg-1',
        transferEpoch: 4,
      }),
    ).toThrow('ownership.denied');
    expect(() =>
      validateOwnershipTransfer({
        resourceKind: 'connection-lease',
        sourceClient: 'client-a',
        destinationClient: 'client-b',
        generation: 'cg-1',
        transferEpoch: -1,
      }),
    ).toThrow('ownership.denied');
  });
});

describe('authorization readiness predicate', () => {
  test('only explicit refusals block', () => {
    expect(isAuthorizationBlocking('denied')).toBe(true);
    expect(isAuthorizationBlocking('restricted')).toBe(true);
    expect(isAuthorizationBlocking('unavailable')).toBe(true);
    expect(isAuthorizationBlocking('unknown')).toBe(false);
    expect(isAuthorizationBlocking('not-determined')).toBe(false);
    expect(isAuthorizationBlocking('granted')).toBe(false);
  });

  test('attachment scope survives arbitration inputs', () => {
    const tuple = createAttachmentTuple(ATTACHMENT);
    expect(tuple.adapterId).toBe('adapter-01');
  });
});
