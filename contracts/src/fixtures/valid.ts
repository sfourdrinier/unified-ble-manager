// contracts/src/fixtures/valid.ts — C-UBM DRAFT valid fixtures.

import { freezeTable } from '../freeze';

export interface ValidFixture {
  readonly name: string;
  readonly kind: string;
  readonly value: unknown;
}

export const VALID_FIXTURES: readonly ValidFixture[] = freezeTable([
  { name: 'attachment-baseline', kind: 'attachment', value: { attachmentId: 'attach-01', backendInstanceId: 'backend-01', backendGeneration: 'bg-3', adapterId: 'adapter-01', adapterGeneration: 'ag-2' } },
  { name: 'peer-public-address', kind: 'peer', value: { domain: 'public-address', value: 'AA:BB:CC:DD:EE:FF' } },
  { name: 'peer-opaque-token', kind: 'peer', value: { domain: 'opaque-token', value: 'token-9f2' } },
  { name: 'uuid-16bit', kind: 'uuid', value: '180D' },
  { name: 'uuid-32bit', kind: 'uuid', value: '12345678' },
  { name: 'uuid-128bit', kind: 'uuid', value: '0000180d-0000-1000-8000-00805f9b34fb' },
  { name: 'ble-address-colon', kind: 'ble-address', value: 'AA:BB:CC:DD:EE:FF' },
  { name: 'ble-address-dash', kind: 'ble-address', value: 'aa-bb-cc-dd-ee-ff' },
  { name: 'scan-broad', kind: 'scan-request', value: { serviceUuids: [], duplicatePolicy: 'all', mergePolicy: 'none', timeoutMs: 5000, hasAbortSignal: false } },
  { name: 'scan-filtered', kind: 'scan-request', value: { serviceUuids: ['180D', '180F'], duplicatePolicy: 'first', mergePolicy: 'none', timeoutMs: 10000, hasAbortSignal: true } },
  { name: 'scan-merged', kind: 'scan-request', value: { serviceUuids: ['180D'], duplicatePolicy: 'merged', mergePolicy: 'latest-by-timestamp', timeoutMs: 10000, hasAbortSignal: true } },
  { name: 'gatt-path-full', kind: 'gatt-path', value: { connectionGeneration: 'cg-1', databaseGeneration: 'dg-1', serviceUuid: '180D', serviceOccurrence: 1, characteristicUuid: '2A37', characteristicOccurrence: 0, descriptorUuid: '2902', descriptorOccurrence: 0, ownerLease: 'lease-1' } },
  { name: 'gatt-path-service-only', kind: 'gatt-path', value: { connectionGeneration: 'cg-1', databaseGeneration: 'dg-1', serviceUuid: '180D', serviceOccurrence: 0, ownerLease: 'lease-1' } },
  { name: 'stream-limits-notification', kind: 'stream-limits', value: { itemCapacity: 64, byteCapacity: 1048576, reservedControlCapacity: 1, reservedControlBytes: 64 } },
  { name: 'stream-limits-min', kind: 'stream-limits', value: { itemCapacity: 1, byteCapacity: 2, reservedControlCapacity: 1, reservedControlBytes: 1 } },
  { name: 'version-overlap', kind: 'version-span', value: { axis: 'backend-contract', minimum: 1, maximum: 3 } },
  { name: 'version-single', kind: 'version-span', value: { axis: 'native-protocol', minimum: 2, maximum: 2 } },
  { name: 'timeout-max', kind: 'timeout', value: 2147483647 },
  { name: 'timeout-min', kind: 'timeout', value: 1 },
  { name: 'deadline-pair', kind: 'deadline', value: { nowMs: 1000, timeoutMs: 500 } },
  { name: 'u64-zero', kind: 'u64', value: '0' },
  { name: 'u64-i64-shared-max', kind: 'u64', value: '9223372036854775807' },
  { name: 'u64-max', kind: 'u64', value: '18446744073709551615' },
  { name: 'i64-min', kind: 'i64', value: '-9223372036854775808' },
  { name: 'i64-max', kind: 'i64', value: '9223372036854775807' },
  { name: 'i64-negative-one', kind: 'i64', value: '-1' },
  { name: 'capability-supported', kind: 'capability', value: { id: 'central.scan', state: 'supported', limits: {}, limitations: [], evidence: { receiptId: 'receipt-1', evidenceLevel: 'deterministic', implementationVersion: '4.0.28', sourceDigest: 'digest-1', scenarioIds: ['scan-start'] } } },
  { name: 'transfer-record', kind: 'ownership-transfer', value: { resourceKind: 'connection-lease', sourceClient: 'client-a', destinationClient: 'client-b', generation: 'cg-1', transferEpoch: 4 } },
  { name: 'peripheral-service', kind: 'peripheral-service', value: { uuid: '180D', primary: true, characteristics: [] } },
  { name: 'zero-length-write', kind: 'write-length', value: 0 },
  { name: 'profile-codec-truncated', kind: 'profile-codec-code', value: 'profile.codec.truncated' },
  { name: 'profile-codec-malformed', kind: 'profile-codec-code', value: 'profile.codec.malformed' },
  { name: 'profile-codec-reserved', kind: 'profile-codec-code', value: 'profile.codec.reserved' },
  { name: 'profile-codec-invalid-value', kind: 'profile-codec-code', value: 'profile.codec.invalid-value' },
] satisfies readonly ValidFixture[]);
