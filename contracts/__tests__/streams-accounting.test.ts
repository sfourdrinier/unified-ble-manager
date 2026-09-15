// contracts/__tests__/streams-accounting.test.ts — C-UBM DRAFT U1 tests.
// Mandatory scenario link: STR-01.

import {
  RESERVED_CONTROL_CAPACITY,
  STREAM_DEFAULTS,
  applyStreamAdmission,
  validateStreamLimits,
} from '../src/index';

describe('bounded stream defaults', () => {
  test('freezes per-stream item, byte, and policy defaults', () => {
    const scan = STREAM_DEFAULTS.find(entry => entry.stream === 'scan-observation');
    if (scan === undefined) {
      throw new Error('scan-observation defaults must be frozen');
    }
    expect(scan.itemCapacity).toBe(1);
    expect(scan.byteCapacity).toBe(524288);
    expect(scan.policy).toBe('latest');
    const notification = STREAM_DEFAULTS.find(entry => entry.stream === 'notification');
    if (notification === undefined) {
      throw new Error('notification defaults must be frozen');
    }
    expect(notification.itemCapacity).toBe(64);
    expect(notification.byteCapacity).toBe(1048576);
    expect(notification.policy).toBe('drop-oldest');
    const replay = STREAM_DEFAULTS.find(entry => entry.stream === 'restoration-replay');
    if (replay === undefined) {
      throw new Error('restoration-replay defaults must be frozen');
    }
    expect(replay.policy).toBe('error');
  });

  test('reserves one control slot so loss accounting survives saturation', () => {
    expect(RESERVED_CONTROL_CAPACITY).toBe(1);
  });

  test('validates limits before registration', () => {
    expect(() =>
      validateStreamLimits({ itemCapacity: 64, byteCapacity: 65536, reservedControlCapacity: 1 }),
    ).not.toThrow();
    expect(() =>
      validateStreamLimits({ itemCapacity: 0, byteCapacity: 65536, reservedControlCapacity: 1 }),
    ).toThrow('argument.invalid');
    expect(() =>
      validateStreamLimits({ itemCapacity: 64, byteCapacity: 1, reservedControlCapacity: 1 }),
    ).toThrow('stream.quota');
  });
});

describe('overflow accounting (STR-01)', () => {
  test('drop-oldest keeps the stream active with cumulative counters', () => {
    const first = applyStreamAdmission(
      { admitted: 1, droppedOldest: 0, droppedBytes: 0, replaced: 0, terminated: false },
      { policy: 'drop-oldest', atCapacity: true, incomingBytes: 100 },
    );
    expect(first.decision).toBe('drop-oldest');
    expect(first.accounting.droppedOldest).toBe(1);
    expect(first.accounting.droppedBytes).toBe(100);
    expect(first.accounting.terminated).toBe(false);
  });

  test('error policy fail-closes the stream once', () => {
    const first = applyStreamAdmission(
      { admitted: 64, droppedOldest: 0, droppedBytes: 0, replaced: 0, terminated: false },
      { policy: 'error', atCapacity: true, incomingBytes: 20 },
    );
    expect(first.decision).toBe('terminate');
    expect(first.accounting.terminated).toBe(true);
  });

  test('latest replaces only the same key without reordering', () => {
    const first = applyStreamAdmission(
      { admitted: 1, droppedOldest: 0, droppedBytes: 0, replaced: 0, terminated: false },
      { policy: 'latest', atCapacity: true, incomingBytes: 50 },
    );
    expect(first.decision).toBe('replace');
    expect(first.accounting.replaced).toBe(1);
    expect(first.accounting.droppedOldest).toBe(0);
  });

  test('admission below capacity neither drops nor terminates', () => {
    const first = applyStreamAdmission(
      { admitted: 1, droppedOldest: 2, droppedBytes: 200, replaced: 1, terminated: false },
      { policy: 'drop-oldest', atCapacity: false, incomingBytes: 10 },
    );
    expect(first.decision).toBe('admit');
    expect(first.accounting.droppedOldest).toBe(2);
    expect(first.accounting.replaced).toBe(1);
  });

  test('counters never decrease across admissions', () => {
    const start = { admitted: 5, droppedOldest: 1, droppedBytes: 50, replaced: 0, terminated: false };
    const next = applyStreamAdmission(start, {
      policy: 'drop-newest',
      atCapacity: true,
      incomingBytes: 30,
    });
    expect(next.accounting.droppedOldest >= start.droppedOldest).toBe(true);
    expect(next.accounting.droppedBytes >= start.droppedBytes).toBe(true);
  });
});
