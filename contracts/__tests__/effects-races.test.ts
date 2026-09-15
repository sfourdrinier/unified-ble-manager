// contracts/__tests__/effects-races.test.ts — C-UBM DRAFT U1 tests.
// Mandatory scenario links: OPS-01, OPS-02, OWN-03.

import { HAPPENS_BEFORE, arbitrateContenders, assertHandshakeBeforeEffects } from '../src/index';

describe('pure input/effect arbitration', () => {
  test('queued abort wins before dispatch and never reaches the radio (OPS-01)', () => {
    const completion = arbitrateContenders({
      operationId: 'op-1',
      dispatched: false,
      contenders: [
        { ingressOrdinal: 1, kind: 'abort', valid: true },
        { ingressOrdinal: 2, kind: 'dispatch-begin', valid: true },
      ],
    });
    expect(completion.winner?.kind).toBe('abort');
    expect(completion.reachedRadio).toBe(false);
    expect(completion.terminal).toBe('aborted');
  });

  test('first valid contender wins; later success is suppressed (OWN-03)', () => {
    const completion = arbitrateContenders({
      operationId: 'op-2',
      dispatched: true,
      contenders: [
        { ingressOrdinal: 1, kind: 'abort', valid: true },
        { ingressOrdinal: 2, kind: 'success', valid: true },
      ],
    });
    expect(completion.winner?.kind).toBe('abort');
    expect(completion.suppressed.length).toBe(1);
    expect(completion.terminal).toBe('aborted');
  });

  test('validated success wins when it arrives first', () => {
    const completion = arbitrateContenders({
      operationId: 'op-3',
      dispatched: true,
      contenders: [
        { ingressOrdinal: 1, kind: 'success', valid: true },
        { ingressOrdinal: 2, kind: 'timeout', valid: true },
      ],
    });
    expect(completion.winner?.kind).toBe('success');
    expect(completion.terminal).toBe('succeeded');
  });

  test('invalid and stale contenders never win', () => {
    const completion = arbitrateContenders({
      operationId: 'op-4',
      dispatched: true,
      contenders: [
        { ingressOrdinal: 1, kind: 'success', valid: false },
        { ingressOrdinal: 2, kind: 'disconnect', valid: true },
      ],
    });
    expect(completion.winner?.kind).toBe('disconnect');
    expect(completion.terminal).toBe('disconnected');
  });

  test('disconnect before settlement invalidates dependent paths', () => {
    const completion = arbitrateContenders({
      operationId: 'op-5',
      dispatched: true,
      contenders: [
        { ingressOrdinal: 1, kind: 'disconnect', valid: true },
        { ingressOrdinal: 2, kind: 'success', valid: true },
      ],
    });
    expect(completion.winner?.kind).toBe('disconnect');
    expect(completion.pathsInvalidBeforeSettlement).toBe(true);
  });

  test('uncertain non-idempotent writes stay explicit (OPS-02)', () => {
    const completion = arbitrateContenders({
      operationId: 'op-6',
      dispatched: true,
      contenders: [{ ingressOrdinal: 1, kind: 'timeout', valid: true }],
    });
    expect(completion.terminal).toBe('timed-out');
    expect(completion.commitState).toBe('unknown');
  });

  test('destroy admission closure suppresses late callbacks', () => {
    const completion = arbitrateContenders({
      operationId: 'op-7',
      dispatched: true,
      contenders: [
        { ingressOrdinal: 1, kind: 'destroy', valid: true },
        { ingressOrdinal: 2, kind: 'success', valid: true },
      ],
    });
    expect(completion.winner?.kind).toBe('destroy');
    expect(completion.terminal).toBe('destroyed');
  });
});

describe('happens-before rules', () => {
  test('publishes the mandatory ordering pairs', () => {
    const pairs = HAPPENS_BEFORE.map(pair => pair.join(' -> '));
    expect(pairs.includes('negotiated-version -> all-work')).toBe(true);
    expect(pairs.includes('ownership-verification -> admission')).toBe(true);
    expect(pairs.includes('generation-invalidation -> terminal-event')).toBe(true);
    expect(pairs.includes('stream-ingress-closure -> stop-resolution')).toBe(true);
    expect(pairs.includes('cleanup-completion -> ownership-release')).toBe(true);
  });
});

describe('effect gating', () => {
  test('no effect dispatches before handshake verification', () => {
    let dispatched = false;
    const dispatch = (): void => {
      dispatched = true;
    };
    expect(() => assertHandshakeBeforeEffects({ complete: false }, 'connect', dispatch)).toThrow(
      'lifecycle.invalid-state',
    );
    expect(dispatched).toBe(false);
  });

  test('verified handshakes run the effect', () => {
    let dispatched = false;
    assertHandshakeBeforeEffects({ complete: true }, 'connect', () => {
      dispatched = true;
    });
    expect(dispatched).toBe(true);
  });
});
