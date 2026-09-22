// contracts/__tests__/outcomes-errors.test.ts — C-UBM DRAFT U1 tests.

import {
  ContractError,
  ERROR_CODE_LIST,
  ERROR_DOMAIN_LIST,
  contractError,
  isBleErrorCode,
  isBleErrorDomain,
  isContractError,
  makePlatformDetail,
  makeTerminalRecord,
  recoveryFor,
} from '../src/index';
import { BLE_ERROR_CODES } from '../../src/backend-contract/errors';

const REQUIRED_BASE_CODES = [
  'protocol.incompatible',
  'protocol.malformed',
  'protocol.violation',
  'lifecycle.destroyed',
  'lifecycle.invalid-state',
  'lifecycle.invariant-violation',
  'backend.reset',
  'adapter.unavailable',
  'adapter.powered-off',
  'adapter.selection-required',
  'adapter.ambiguous',
  'permission.denied',
  'permission.restricted',
  'permission.not-determined',
  'ownership.denied',
  'connection.already-owned',
  'scan.already-active',
  'chooser.busy',
  'argument.invalid',
  'bytes.invalid',
  'bytes.too-large',
  'connection.not-found',
  'connection.failed',
  'connection.stale',
  'connection.lost',
  'operation.aborted',
  'operation.timed-out',
  'operation.disconnected',
  'operation.cancelled-by-destroy',
  'operation.reset',
  'operation.adapter-unavailable',
  'gatt.discovery-required',
  'gatt.ambiguous-path',
  'gatt.stale-handle',
  'gatt.cache-unknown',
  'gatt.not-found',
  'gatt.property-not-supported',
  'gatt.read-failed',
  'gatt.write-failed',
  'gatt.subscribe-failed',
  'gatt.cccd-managed',
  'stream.overflow',
  'stream.closed',
  'stream.quota',
  'stream.rate-limited',
  'capability.unsupported',
  'capability.unavailable',
  'capability.limited',
  'background.terminated',
  'platform.failure',
  'platform.security',
  'platform.transport',
];

describe('error identity catalog', () => {
  test('covers every mandatory base code', () => {
    for (const code of REQUIRED_BASE_CODES) {
      expect(isBleErrorCode(code)).toBe(true);
      if (isBleErrorCode(code)) {
        expect(ERROR_CODE_LIST.includes(code)).toBe(true);
      }
    }
  });

  test('rejects unknown codes', () => {
    expect(isBleErrorCode('nope.unknown')).toBe(false);
    expect(isBleErrorCode('')).toBe(false);
    expect(isBleErrorCode(42)).toBe(false);
    expect(isBleErrorCode(null)).toBe(false);
  });

  test('domains are the frozen 4.x domain set', () => {
    expect(ERROR_DOMAIN_LIST.includes('core')).toBe(true);
    expect(ERROR_DOMAIN_LIST.includes('platform')).toBe(true);
    expect(isBleErrorDomain('gatt')).toBe(true);
    expect(isBleErrorDomain('nope')).toBe(false);
  });

  test('every listed code is guarded consistently', () => {
    for (const code of ERROR_CODE_LIST) {
      expect(isBleErrorCode(code)).toBe(true);
    }
  });

  test('matches the 4.x oracle catalog verbatim (67/67, LOW-1)', () => {
    const oracle: readonly string[] = BLE_ERROR_CODES;
    expect(oracle.length).toBe(67);
    expect(ERROR_CODE_LIST.length).toBe(67);
    expect([...ERROR_CODE_LIST]).toEqual([...oracle]);
  });
});

describe('contract errors', () => {
  test('carry code, domain, operation, and recoverability', () => {
    const error = contractError('scan.already-active', 'scan', 'scan.start');
    expect(error).toBeInstanceOf(ContractError);
    expect(error).toBeInstanceOf(Error);
    expect(error.code).toBe('scan.already-active');
    expect(error.domain).toBe('scan');
    expect(error.operation).toBe('scan.start');
    expect(error.message).toContain('scan.already-active');
  });

  test('guards accept only genuine contract errors', () => {
    expect(isContractError(contractError('argument.invalid', 'core', 'op'))).toBe(true);
    expect(isContractError(new Error('argument.invalid'))).toBe(false);
    expect(isContractError(null)).toBe(false);
    expect(isContractError({ code: 'argument.invalid' })).toBe(false);
  });

  test('rejects empty operations', () => {
    expect(() => contractError('argument.invalid', 'core', '')).toThrow('argument.invalid');
  });
});

describe('platform detail redaction', () => {
  test('keeps only safe fields', () => {
    const detail = makePlatformDetail({
      domain: 'corebluetooth',
      code: '7',
      safeMessage: 'request rejected',
    });
    expect(detail.domain).toBe('corebluetooth');
    expect(detail.code).toBe('7');
    expect(detail.safeMessage).toBe('request rejected');
  });

  test('rejects empty or non-string safe fields', () => {
    expect(() => makePlatformDetail({ domain: '', code: '7', safeMessage: 'x' })).toThrow(
      'argument.invalid',
    );
    expect(() => makePlatformDetail({ domain: 'd', code: '7', safeMessage: '' })).toThrow(
      'argument.invalid',
    );
  });
});

describe('recovery dispositions', () => {
  test('validation failures never retry', () => {
    expect(recoveryFor('argument.invalid').disposition).toBe('none');
    expect(recoveryFor('protocol.incompatible').disposition).toBe('none');
  });

  test('destroyed work recreates the manager', () => {
    const recovery = recoveryFor('lifecycle.destroyed');
    expect(recovery.actions.includes('recreate-manager')).toBe(true);
  });

  test('stale handles rediscover GATT', () => {
    const recovery = recoveryFor('gatt.stale-handle');
    expect(recovery.disposition).toBe('retry-immediately');
    expect(recovery.actions.includes('rediscover-gatt')).toBe(true);
  });

  test('unsupported capabilities never retry', () => {
    expect(recoveryFor('capability.unsupported').disposition).toBe('none');
    expect(recoveryFor('capability.unavailable').disposition).toBe('none');
  });
});

describe('terminal records', () => {
  test('records one immutable terminal outcome', () => {
    const record = makeTerminalRecord({
      operationId: 'op-1',
      kind: 'succeeded',
      cause: null,
      ingressOrdinal: 3,
      startedAt: 100,
      settledAt: 150,
    });
    expect(record.kind).toBe('succeeded');
    expect(record.cause).toBe(null);
    expect(record.ingressOrdinal).toBe(3);
    expect(Object.isFrozen(record)).toBe(true);
  });

  test('failed terminals carry their cause code', () => {
    const record = makeTerminalRecord({
      operationId: 'op-2',
      kind: 'aborted',
      cause: 'operation.aborted',
      ingressOrdinal: 4,
      startedAt: 100,
      settledAt: 120,
    });
    expect(record.cause).toBe('operation.aborted');
  });

  test('rejects unsettled timing and unknown kinds', () => {
    expect(() =>
      makeTerminalRecord({
        operationId: 'op-3',
        kind: 'succeeded',
        cause: null,
        ingressOrdinal: 1,
        startedAt: 200,
        settledAt: 100,
      }),
    ).toThrow('argument.invalid');
    expect(() =>
      makeTerminalRecord({
        operationId: '',
        kind: 'failed',
        cause: 'platform.failure',
        ingressOrdinal: 1,
        startedAt: 100,
        settledAt: 100,
      }),
    ).toThrow('argument.invalid');
  });
});
