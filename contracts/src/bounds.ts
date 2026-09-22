// contracts/src/bounds.ts — C-UBM DRAFT (pending U1 acceptance).
//
// Frozen numeric production limits, monotonic deadline semantics, and 64-bit
// numeric mappings. Derived from docs/UNIFIED_SEMANTICS.md §11/§12/§13 and
// src/public/{stream-capacity,scan-state-budget,operation-options}.ts plus
// src/manager/ble-manager.ts aggregates (read-only reference).

import { contractError } from './outcomes';

export const MAX_STREAM_ITEM_CAPACITY: 65536 = 65536;
export const MIN_STREAM_ITEM_CAPACITY: 1 = 1;
export const MAX_STREAM_BYTE_CAPACITY: 4194304 = 4194304;
export const CLIENT_AGGREGATE_BYTES: 4194304 = 4194304;
export const BACKEND_INGRESS_AGGREGATE_BYTES: 16777216 = 16777216;
export const ADAPTER_OWNER_AGGREGATE_BYTES: 67108864 = 67108864;
export const MAX_OPERATION_BYTES: 524288 = 524288;
export const MAX_SCAN_STATE_ENTRIES: 256 = 256;
export const MAX_SCAN_STATE_BYTES: 262144 = 262144;
export const TRACE_MAX_BYTES: 524288 = 524288;
export const MAX_TIMEOUT_MS: 2147483647 = 2147483647;
export const MIN_TIMEOUT_MS: 1 = 1;
export const MAX_IPC_LEASES_PER_IDENTITY: 2 = 2;

export function assertItemCapacity(value: unknown, operation: string): number {
  if (
    typeof value !== 'number' ||
    !Number.isSafeInteger(value) ||
    value < MIN_STREAM_ITEM_CAPACITY ||
    value > MAX_STREAM_ITEM_CAPACITY
  ) {
    throw contractError('argument.invalid', 'stream', operation);
  }
  return value;
}

export function assertByteCapacity(value: unknown, operation: string): number {
  if (typeof value !== 'number' || !Number.isSafeInteger(value) || value < 1) {
    throw contractError('argument.invalid', 'stream', operation);
  }
  if (value > MAX_STREAM_BYTE_CAPACITY) {
    throw contractError('stream.quota', 'stream', operation);
  }
  return value;
}

export function assertTimeoutMs(value: unknown, operation: string): number {
  if (
    typeof value !== 'number' ||
    !Number.isSafeInteger(value) ||
    value < MIN_TIMEOUT_MS ||
    value > MAX_TIMEOUT_MS
  ) {
    throw contractError('argument.invalid', 'core', operation);
  }
  return value;
}

export function isMonotonicNow(value: unknown): value is number {
  return typeof value === 'number' && Number.isFinite(value) && value >= 0;
}

// Deadlines are absolute monotonic instants. A duration converts to one at
// request admission; helpers preserve the earlier deadline, never extend it.
export function toDeadline(nowMs: number, timeoutMs: number): number {
  if (!isMonotonicNow(nowMs)) {
    throw contractError('argument.invalid', 'core', 'deadline.now');
  }
  assertTimeoutMs(timeoutMs, 'deadline.timeout');
  const absolute = nowMs + timeoutMs;
  if (!Number.isFinite(absolute) || absolute < 0) {
    throw contractError('argument.invalid', 'core', 'deadline.absolute');
  }
  return absolute;
}

export function isDeadlineExpired(nowMs: number, deadlineMs: number): boolean {
  if (!isMonotonicNow(nowMs) || !isMonotonicNow(deadlineMs)) {
    throw contractError('argument.invalid', 'core', 'deadline.compare');
  }
  return nowMs >= deadlineMs;
}

export function earliestDeadline(first: number, second: number): number {
  if (!isMonotonicNow(first) || !isMonotonicNow(second)) {
    throw contractError('argument.invalid', 'core', 'deadline.compose');
  }
  return first < second ? first : second;
}

// The effective maximum is the minimum of the operation, negotiated, adapter,
// and protocol maxima, clamped to the frozen operation ceiling. An
// unavailable or unmeasured maximum is not infinity.
export function effectiveMaxBytes(maxima: readonly number[]): number {
  if (maxima.length === 0) {
    throw contractError('argument.invalid', 'core', 'bytes.maxima');
  }
  let effective: number = MAX_OPERATION_BYTES;
  for (const maximum of maxima) {
    if (typeof maximum !== 'number' || !Number.isSafeInteger(maximum) || maximum <= 0) {
      throw contractError('argument.invalid', 'core', 'bytes.maxima');
    }
    if (maximum < effective) {
      effective = maximum;
    }
  }
  return effective;
}

export function assertBytesWithinLimit(
  length: number,
  maxima: readonly (number | null)[],
  operation: string,
): void {
  if (typeof length !== 'number' || !Number.isSafeInteger(length) || length < 0) {
    throw contractError('bytes.invalid', 'core', operation);
  }
  const declared: number[] = [];
  for (const maximum of maxima) {
    if (maximum === null) {
      throw contractError('capability.unavailable', 'capability', operation);
    }
    declared.push(maximum);
  }
  if (length > effectiveMaxBytes(declared)) {
    throw contractError('bytes.too-large', 'core', operation);
  }
}

// DATA-02: large counters and fractional aligned times cross bindings without
// precision loss. JS integers stay within the safe range; 64-bit wire values
// cross as decimal strings and validate as bigint.
//
// Canonical decimal form: `^-?[0-9]+$` with no plus sign, no whitespace, no
// hex/fraction/exponent, and at most MAX_DECIMAL_DIGITS digits excluding an
// optional leading `-`. Longer digit strings are rejected as `bytes.invalid`
// before range checks; the Rust mirror enforces the identical form.
export const U64_MAX: bigint = 18446744073709551615n;
export const U64_MIN: bigint = 0n;
export const I64_MAX: bigint = 9223372036854775807n;
export const I64_MIN: bigint = -9223372036854775808n;
export const MAX_DECIMAL_DIGITS: 20 = 20;

export function isSafeJsInteger(value: unknown): value is number {
  return typeof value === 'number' && Number.isSafeInteger(value);
}

function assertDecimalDigitCap(digits: string, path: string): void {
  if (digits.length > MAX_DECIMAL_DIGITS) {
    throw contractError('bytes.invalid', 'core', path);
  }
}

export function parseU64Decimal(value: unknown): bigint {
  if (typeof value !== 'string' || !/^[0-9]+$/.test(value)) {
    throw contractError('bytes.invalid', 'core', 'u64.input');
  }
  assertDecimalDigitCap(value, 'u64.input');
  const parsed = BigInt(value);
  if (parsed < U64_MIN || parsed > U64_MAX) {
    throw contractError('bytes.invalid', 'core', 'u64.range');
  }
  return parsed;
}

export function parseI64Decimal(value: unknown): bigint {
  if (typeof value !== 'string' || !/^-?[0-9]+$/.test(value)) {
    throw contractError('bytes.invalid', 'core', 'i64.input');
  }
  const digits = value.startsWith('-') ? value.slice(1) : value;
  assertDecimalDigitCap(digits, 'i64.input');
  const parsed = BigInt(value);
  if (parsed < I64_MIN || parsed > I64_MAX) {
    throw contractError('bytes.invalid', 'core', 'i64.range');
  }
  return parsed;
}
