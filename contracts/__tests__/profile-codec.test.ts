// contracts/__tests__/profile-codec.test.ts — C-UBM 0.1.2 additive amendment.
//
// Freezes the `profile.codec.*` SIG payload-codec identities into the contract
// catalog with the exact wire strings of the retained oracle
// (`src/profiles/errors.ts#ProfileCodecErrorCode`, read-only reference).
// Test-first: every test below fails until contracts/src/outcomes.ts exposes
// the frozen `PROFILE_CODEC_ERROR_CODES` table plus its guard.

import {
  PROFILE_CODEC_ERROR_CODES,
  isProfileCodecErrorCode,
} from '../src/index';

// Byte-identical copies of the retained oracle identities
// (`src/profiles/errors.ts`). Any drift fails closed here.
const ORACLE_WIRE_STRINGS: readonly string[] = [
  'profile.codec.truncated',
  'profile.codec.malformed',
  'profile.codec.reserved',
  'profile.codec.invalid-value',
];

function cloneViaJson(value: unknown): unknown {
  return JSON.parse(JSON.stringify(value));
}

describe('profile codec identity catalog (0.1.2 additive amendment)', () => {
  test('freezes exactly the four oracle identities in oracle order', () => {
    expect([...PROFILE_CODEC_ERROR_CODES]).toEqual([...ORACLE_WIRE_STRINGS]);
  });

  test('every identity resolves through the guard (wire-exact)', () => {
    for (const code of ORACLE_WIRE_STRINGS) {
      expect(isProfileCodecErrorCode(code)).toBe(true);
    }
    for (const code of PROFILE_CODEC_ERROR_CODES) {
      expect(isProfileCodecErrorCode(code)).toBe(true);
      expect(ORACLE_WIRE_STRINGS.includes(code)).toBe(true);
    }
  });

  test('rejects unknown codes, case drift, and non-strings', () => {
    expect(isProfileCodecErrorCode('profile.codec.unknown')).toBe(false);
    expect(isProfileCodecErrorCode('profile.codec')).toBe(false);
    expect(isProfileCodecErrorCode('Profile.Codec.Truncated')).toBe(false);
    expect(isProfileCodecErrorCode('')).toBe(false);
    expect(isProfileCodecErrorCode(42)).toBe(false);
    expect(isProfileCodecErrorCode(null)).toBe(false);
  });

  test('the frozen table is runtime-immutable', () => {
    expect(Object.isFrozen(PROFILE_CODEC_ERROR_CODES)).toBe(true);
  });

  test('identities survive JSON wire round trips unchanged', () => {
    for (const code of PROFILE_CODEC_ERROR_CODES) {
      const revived: unknown = cloneViaJson(code);
      expect(revived).toBe(code);
      expect(isProfileCodecErrorCode(revived)).toBe(true);
    }
  });
});
