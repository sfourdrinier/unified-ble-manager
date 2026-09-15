// contracts/src/fixtures/invalid.ts — C-UBM DRAFT invalid fixtures.
// Each entry must throw its frozen code; enforced by fixtures-roundtrip.test.ts.

import { assertBytesWithinLimit, assertItemCapacity, assertTimeoutMs, parseI64Decimal, parseU64Decimal } from '../bounds';
import { assertCapabilityAllows, makeCapabilityDescriptor } from '../capabilities';
import { validateScanRequest } from '../central';
import { canonicalBleAddressValue, canonicalUuidValue, createAttachmentTuple, createPeerIdentity } from '../identities';
import { makeVersionSpan, negotiateVersionSpan } from '../version';
import type { BleErrorCode } from '../outcomes';

export interface InvalidFixture {
  readonly name: string;
  readonly expectedCode: BleErrorCode;
  readonly check: () => void;
}

const UNSUPPORTED_CAPABILITY = makeCapabilityDescriptor({
  id: 'connection.priority',
  state: 'unsupported',
  limits: {},
  limitations: [{ code: 'no-surface', explanation: 'platform exposes none', affectedGuarantee: 'priority' }],
  evidence: { receiptId: 'r', evidenceLevel: 'blocked', implementationVersion: '4.0.28', sourceDigest: 'd', scenarioIds: [] },
});

const BASE_ATTACHMENT = {
  attachmentId: 'attach-01',
  backendInstanceId: 'backend-01',
  backendGeneration: 'bg-3',
  adapterId: 'adapter-01',
  adapterGeneration: 'ag-2',
};

export const INVALID_FIXTURES: readonly InvalidFixture[] = [
  { name: 'empty-attachment-id', expectedCode: 'argument.invalid', check: () => { createAttachmentTuple({ ...BASE_ATTACHMENT, attachmentId: '' }); } },
  { name: 'uuid-garbage', expectedCode: 'argument.invalid', check: () => { canonicalUuidValue('not-a-uuid'); } },
  { name: 'uuid-bad-length', expectedCode: 'argument.invalid', check: () => { canonicalUuidValue('123'); } },
  { name: 'ble-address-short', expectedCode: 'argument.invalid', check: () => { canonicalBleAddressValue('AA:BB:CC'); } },
  { name: 'peer-empty-value', expectedCode: 'argument.invalid', check: () => { createPeerIdentity(createAttachmentTuple(BASE_ATTACHMENT), 'opaque-token', ''); } },
  { name: 'version-range-inverted', expectedCode: 'protocol.malformed', check: () => { makeVersionSpan('backend-contract', 3, 2); } },
  {
    name: 'version-no-overlap',
    expectedCode: 'protocol.incompatible',
    check: () => {
      negotiateVersionSpan(
        makeVersionSpan('backend-contract', 1, 1),
        makeVersionSpan('backend-contract', 2, 2),
      );
    },
  },
  {
    name: 'scan-unsupported-filter',
    expectedCode: 'capability.unsupported',
    check: () => {
      validateScanRequest({ serviceUuids: [], duplicatePolicy: 'all', mergePolicy: 'none', timeoutMs: 1000, hasAbortSignal: false, unsupportedFilterFields: ['byte-predicate'] });
    },
  },
  {
    name: 'capability-unsupported-request',
    expectedCode: 'capability.unsupported',
    check: () => { assertCapabilityAllows(UNSUPPORTED_CAPABILITY, 'connection.requestPriority'); },
  },
  { name: 'item-capacity-zero', expectedCode: 'argument.invalid', check: () => { assertItemCapacity(0, 'scan'); } },
  { name: 'item-capacity-huge', expectedCode: 'argument.invalid', check: () => { assertItemCapacity(65537, 'scan'); } },
  { name: 'timeout-zero', expectedCode: 'argument.invalid', check: () => { assertTimeoutMs(0, 'op'); } },
  { name: 'timeout-int32-overflow', expectedCode: 'argument.invalid', check: () => { assertTimeoutMs(2147483648, 'op'); } },
  {
    name: 'bytes-over-operation-max',
    expectedCode: 'bytes.too-large',
    check: () => { assertBytesWithinLimit(524289, [524288], 'write'); },
  },
  { name: 'u64-overflow', expectedCode: 'bytes.invalid', check: () => { parseU64Decimal('18446744073709551616'); } },
  { name: 'u64-negative', expectedCode: 'bytes.invalid', check: () => { parseU64Decimal('-1'); } },
  { name: 'u64-fraction', expectedCode: 'bytes.invalid', check: () => { parseU64Decimal('1.5'); } },
  { name: 'i64-overflow', expectedCode: 'bytes.invalid', check: () => { parseI64Decimal('9223372036854775808'); } },
  { name: 'i64-underflow', expectedCode: 'bytes.invalid', check: () => { parseI64Decimal('-9223372036854775809'); } },
  { name: 'i64-hex', expectedCode: 'bytes.invalid', check: () => { parseI64Decimal('0x10'); } },
] satisfies readonly InvalidFixture[];
