// contracts/__tests__/bounds-deadlines-numerics.test.ts — C-UBM DRAFT U1 tests.

import {
  ADAPTER_OWNER_AGGREGATE_BYTES,
  BACKEND_INGRESS_AGGREGATE_BYTES,
  CLIENT_AGGREGATE_BYTES,
  I64_MAX,
  I64_MIN,
  MAX_IPC_LEASES_PER_IDENTITY,
  MAX_OPERATION_BYTES,
  MAX_SCAN_STATE_BYTES,
  MAX_SCAN_STATE_ENTRIES,
  MAX_STREAM_BYTE_CAPACITY,
  MAX_STREAM_ITEM_CAPACITY,
  MAX_TIMEOUT_MS,
  MIN_STREAM_ITEM_CAPACITY,
  U64_MAX,
  assertByteCapacity,
  assertBytesWithinLimit,
  assertItemCapacity,
  assertTimeoutMs,
  earliestDeadline,
  effectiveMaxBytes,
  isDeadlineExpired,
  isMonotonicNow,
  isSafeJsInteger,
  parseI64Decimal,
  parseU64Decimal,
  toDeadline,
} from '../src/index';

describe('frozen numeric production limits', () => {
  test('pins stream and aggregate budgets', () => {
    expect(MAX_STREAM_ITEM_CAPACITY).toBe(65536);
    expect(MIN_STREAM_ITEM_CAPACITY).toBe(1);
    expect(MAX_STREAM_BYTE_CAPACITY).toBe(4194304);
    expect(CLIENT_AGGREGATE_BYTES).toBe(4194304);
    expect(BACKEND_INGRESS_AGGREGATE_BYTES).toBe(16777216);
    expect(ADAPTER_OWNER_AGGREGATE_BYTES).toBe(67108864);
  });

  test('pins operation, scan-state, timeout, and IPC bounds', () => {
    expect(MAX_OPERATION_BYTES).toBe(524288);
    expect(MAX_SCAN_STATE_ENTRIES).toBe(256);
    expect(MAX_SCAN_STATE_BYTES).toBe(262144);
    expect(MAX_TIMEOUT_MS).toBe(2147483647);
    expect(MAX_IPC_LEASES_PER_IDENTITY).toBe(2);
  });

  test('validates capacities fail-closed', () => {
    expect(() => assertItemCapacity(1, 'scan')).not.toThrow();
    expect(() => assertItemCapacity(65536, 'scan')).not.toThrow();
    expect(() => assertItemCapacity(0, 'scan')).toThrow('argument.invalid');
    expect(() => assertItemCapacity(65537, 'scan')).toThrow('argument.invalid');
    expect(() => assertByteCapacity(4194304, 'scan')).not.toThrow();
    expect(() => assertByteCapacity(4194305, 'scan')).toThrow('stream.quota');
    expect(() => assertTimeoutMs(2147483647, 'op')).not.toThrow();
    expect(() => assertTimeoutMs(0, 'op')).toThrow('argument.invalid');
    expect(() => assertTimeoutMs(2147483648, 'op')).toThrow('argument.invalid');
  });

  test('effective maximum is the minimum of declared limits', () => {
    expect(effectiveMaxBytes([524288, 1024, 4096])).toBe(1024);
    expect(() => effectiveMaxBytes([])).toThrow('argument.invalid');
  });

  test('byte admission fails before dispatch; unknown maxima are unavailable', () => {
    expect(() => assertBytesWithinLimit(1024, [524288], 'write')).not.toThrow();
    expect(() => assertBytesWithinLimit(524289, [524288], 'write')).toThrow('bytes.too-large');
    expect(() => assertBytesWithinLimit(8, [null], 'read')).toThrow('capability.unavailable');
    expect(() => assertBytesWithinLimit(0, [524288], 'read')).not.toThrow();
  });
});

describe('monotonic deadlines', () => {
  test('converts durations to absolute instants at admission', () => {
    expect(toDeadline(1000, 500)).toBe(1500);
  });

  test('rejects non-monotonic clocks and bad durations', () => {
    expect(() => toDeadline(-1, 500)).toThrow('argument.invalid');
    expect(() => toDeadline(Number.NaN, 500)).toThrow('argument.invalid');
    expect(() => toDeadline(1000, 0)).toThrow('argument.invalid');
    expect(isMonotonicNow(0)).toBe(true);
    expect(isMonotonicNow(-1)).toBe(false);
    expect(isMonotonicNow(Number.NaN)).toBe(false);
  });

  test('never extends an existing deadline', () => {
    expect(earliestDeadline(1500, 1200)).toBe(1200);
    expect(earliestDeadline(1200, 1500)).toBe(1200);
  });

  test('detects expiry', () => {
    expect(isDeadlineExpired(1500, 1500)).toBe(true);
    expect(isDeadlineExpired(1501, 1500)).toBe(true);
    expect(isDeadlineExpired(1499, 1500)).toBe(false);
  });
});

describe('64-bit numeric mappings (DATA-02)', () => {
  test('pins u64/i64 extremes', () => {
    expect(U64_MAX).toBe(18446744073709551615n);
    expect(I64_MAX).toBe(9223372036854775807n);
    expect(I64_MIN).toBe(-9223372036854775808n);
  });

  test('accepts boundary values in decimal-string wire form', () => {
    expect(parseU64Decimal('0')).toBe(0n);
    expect(parseU64Decimal('9223372036854775807')).toBe(9223372036854775807n);
    expect(parseU64Decimal('18446744073709551615')).toBe(18446744073709551615n);
    expect(parseI64Decimal('-9223372036854775808')).toBe(-9223372036854775808n);
    expect(parseI64Decimal('9223372036854775807')).toBe(9223372036854775807n);
  });

  test('rejects out-of-range and malformed 64-bit values', () => {
    expect(() => parseU64Decimal('18446744073709551616')).toThrow('bytes.invalid');
    expect(() => parseU64Decimal('-1')).toThrow('bytes.invalid');
    expect(() => parseU64Decimal('12.5')).toThrow('bytes.invalid');
    expect(() => parseU64Decimal('')).toThrow('bytes.invalid');
    expect(() => parseI64Decimal('9223372036854775808')).toThrow('bytes.invalid');
    expect(() => parseI64Decimal('-9223372036854775809')).toThrow('bytes.invalid');
    expect(() => parseI64Decimal('0x10')).toThrow('bytes.invalid');
  });

  test('flags JS-unsafe integers before they cross bindings', () => {
    expect(isSafeJsInteger(9007199254740991)).toBe(true);
    expect(isSafeJsInteger(9007199254740992)).toBe(false);
    expect(isSafeJsInteger(1.5)).toBe(false);
  });
});
