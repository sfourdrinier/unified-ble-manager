// contracts/__tests__/semantic-map.test.ts — C-UBM DRAFT U1 tests.
// Every 4.x semantic section maps to C-UBM or an explicit approved correction.
// Disagreements land on the protocol/contract, never as silent equality.

import {
  APPROVED_CORRECTIONS,
  MANDATORY_SCENARIOS,
  SEMANTIC_MAP,
  correctionForMapping,
} from '../src/index';

describe('4.x semantic map', () => {
  test('covers every UNIFIED_SEMANTICS section 1-25', () => {
    for (let section = 1; section <= 25; section += 1) {
      const mapped = SEMANTIC_MAP.filter(entry => entry.section === section);
      expect(mapped.length > 0).toBe(true);
    }
  });

  test('every entry names its C-UBM module', () => {
    for (const entry of SEMANTIC_MAP) {
      expect(entry.cubmModule.length > 0).toBe(true);
      expect(entry.subject.length > 0).toBe(true);
    }
  });

  test('corrected entries reference an approved correction, never silent equality', () => {
    const corrected = SEMANTIC_MAP.filter(entry => entry.disposition === 'corrected');
    expect(corrected.length > 0).toBe(true);
    for (const entry of corrected) {
      const correction = correctionForMapping(entry);
      expect(correction !== null).toBe(true);
    }
  });

  test('every approved correction states the 4.x behavior and the C-UBM decision', () => {
    expect(APPROVED_CORRECTIONS.length > 0).toBe(true);
    for (const correction of APPROVED_CORRECTIONS) {
      expect(correction.id.length > 0).toBe(true);
      expect(correction.fourXBehavior.length > 0).toBe(true);
      expect(correction.cubmDecision.length > 0).toBe(true);
      expect(correction.rationale.length > 0).toBe(true);
    }
  });

  test('links every mandatory CON-UBM scenario', () => {
    const required = ['OWN-01', 'OWN-02', 'CLN-01', 'OPS-01', 'OPS-02', 'STR-01', 'PKG-02'];
    for (const id of required) {
      expect(MANDATORY_SCENARIOS.includes(id)).toBe(true);
    }
  });
});
