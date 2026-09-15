// contracts/__tests__/cleanup-counters.test.ts — C-UBM DRAFT U1 tests.
// Mandatory scenario links: CLN-01, CLN-05.

import {
  combineCleanupRecords,
  createCounterLedger,
  makeCleanupRecord,
} from '../src/index';
import { contractError } from '../src/index';

describe('cleanup receipts', () => {
  test('successful cleanup carries zero failures', () => {
    const record = makeCleanupRecord({ state: 'released', failures: [] });
    expect(record.state).toBe('released');
    expect(record.failures.length).toBe(0);
    expect(Object.isFrozen(record)).toBe(true);
  });

  test('failed cleanup retains every failure (CLN-01)', () => {
    const record = makeCleanupRecord({
      state: 'release-failed',
      failures: [
        { resourceKind: 'physical-link', error: contractError('connection.lost', 'connection', 'disconnect') },
      ],
    });
    expect(record.state).toBe('release-failed');
    expect(record.failures.length).toBe(1);
  });

  test('released with failures is a protocol violation, never silent success', () => {
    expect(() =>
      makeCleanupRecord({
        state: 'released',
        failures: [
          { resourceKind: 'link', error: contractError('connection.lost', 'connection', 'disconnect') },
        ],
      }),
    ).toThrow('protocol.malformed');
    expect(() => makeCleanupRecord({ state: 'release-failed', failures: [] })).toThrow(
      'protocol.malformed',
    );
  });

  test('batch cleanup continues after individual failure and composes one record', () => {
    const ok = makeCleanupRecord({ state: 'released', failures: [] });
    const failed = makeCleanupRecord({
      state: 'release-failed',
      failures: [
        { resourceKind: 'cccd', error: contractError('gatt.subscribe-failed', 'gatt', 'unsubscribe') },
      ],
    });
    const composite = combineCleanupRecords([ok, failed]);
    expect(composite.state).toBe('release-failed');
    expect(composite.failures.length).toBe(1);
  });

  test('repeated cleanup returns the same immutable outcome (CLN-05)', () => {
    const failed = makeCleanupRecord({
      state: 'release-failed',
      failures: [
        { resourceKind: 'link', error: contractError('connection.lost', 'connection', 'disconnect') },
      ],
    });
    const replay = combineCleanupRecords([failed]);
    expect(replay).toEqual(failed);
  });
});

describe('resource counters', () => {
  test('increments before observability and decrements before release', () => {
    const ledger = createCounterLedger();
    const afterIncrement = ledger.increment('connection-leases');
    expect(afterIncrement.count('connection-leases')).toBe(1);
    const afterDecrement = afterIncrement.decrement('connection-leases');
    expect(afterDecrement.count('connection-leases')).toBe(0);
  });

  test('underflow is a protocol failure that forces reset', () => {
    const ledger = createCounterLedger();
    expect(() => ledger.decrement('scan-controllers')).toThrow('lifecycle.invariant-violation');
  });

  test('ledgers are immutable snapshots', () => {
    const ledger = createCounterLedger();
    const next = ledger.increment('subscription-consumers');
    expect(ledger.count('subscription-consumers')).toBe(0);
    expect(next.count('subscription-consumers')).toBe(1);
  });
});
