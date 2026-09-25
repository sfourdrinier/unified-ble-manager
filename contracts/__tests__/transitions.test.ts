// contracts/__tests__/transitions.test.ts — C-UBM DRAFT U1 tests.

import {
  CONTENTION_RULINGS,
  TRANSITION_TABLES,
  isTerminalState,
  isTransitionAllowed,
} from '../src/index';
import type { MachineName } from '../src/index';

describe('lifecycle transition tables', () => {
  test('freezes every §4 machine', () => {
    const names = TRANSITION_TABLES.map(table => table.machine);
    const expected: readonly MachineName[] = [
      'provider',
      'backend',
      'manager',
      'operation',
      'scan-session',
      'chooser-session',
      'connection',
      'database',
      'subscription',
    ];
    for (const name of expected) {
      expect(names.includes(name)).toBe(true);
    }
  });

  test('allows the canonical happy paths', () => {
    expect(isTransitionAllowed('backend', 'created', 'negotiating')).toBe(true);
    expect(isTransitionAllowed('backend', 'negotiating', 'ready')).toBe(true);
    expect(isTransitionAllowed('manager', 'ready', 'destroying')).toBe(true);
    expect(isTransitionAllowed('connection', 'connecting', 'connected')).toBe(true);
    expect(isTransitionAllowed('database', 'discovering', 'current')).toBe(true);
    expect(isTransitionAllowed('subscription', 'enabling', 'ready')).toBe(true);
    expect(isTransitionAllowed('scan-session', 'active', 'stopping')).toBe(true);
  });

  test('a native start refusal after an early stop is a failed scan terminal', () => {
    const scan = TRANSITION_TABLES.find(table => table.machine === 'scan-session');
    expect(scan?.transitions).toContainEqual({
      from: 'stopping',
      to: 'failed',
      via: 'start-failed',
    });
  });

  test('forbids resurrection of stale or destroyed objects', () => {
    expect(isTransitionAllowed('backend', 'stopped', 'ready')).toBe(false);
    expect(isTransitionAllowed('manager', 'destroyed', 'ready')).toBe(false);
    expect(isTransitionAllowed('connection', 'invalid', 'connected')).toBe(false);
    expect(isTransitionAllowed('database', 'invalid', 'current')).toBe(false);
    expect(isTransitionAllowed('subscription', 'removed', 'ready')).toBe(false);
    expect(isTransitionAllowed('operation', 'succeeded', 'failed')).toBe(false);
  });

  test('marks terminal states', () => {
    expect(isTerminalState('manager', 'destroyed')).toBe(true);
    expect(isTerminalState('manager', 'ready')).toBe(false);
    expect(isTerminalState('operation', 'succeeded')).toBe(true);
    expect(isTerminalState('operation', 'dispatched')).toBe(false);
    expect(isTerminalState('connection', 'invalid')).toBe(true);
  });

  test('every transition row is internally consistent', () => {
    for (const table of TRANSITION_TABLES) {
      for (const row of table.transitions) {
        expect(table.states.includes(row.from)).toBe(true);
        expect(table.states.includes(row.to)).toBe(true);
      }
      for (const terminal of table.terminals) {
        expect(table.states.includes(terminal)).toBe(true);
      }
    }
  });
});

describe('contention rulings', () => {
  test('freezes the §3 arbitration outcomes', () => {
    const scan = CONTENTION_RULINGS.find(ruling => ruling.resource === 'ordinary-scan');
    if (scan === undefined) {
      throw new Error('ordinary-scan ruling must be frozen');
    }
    expect(scan.rejection).toBe('scan.already-active');
    const lease = CONTENTION_RULINGS.find(ruling => ruling.resource === 'peer-connection-shared');
    if (lease === undefined) {
      throw new Error('peer-connection-shared ruling must be frozen');
    }
    expect(lease.rejection).toBe(null);
  });
});
