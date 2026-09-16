// contracts/__tests__/profile-codec.test.ts — C-UBM 0.1.2 additive amendment.
//
// Freezes the `profile.codec.*` SIG payload-codec identities into the contract
// catalog against the live retained oracle
// (`src/profiles/errors.ts#PROFILE_CODEC_ERROR_CODES`): any oracle drift
// fails closed here, mirroring the 67/67 live-oracle convention in
// `outcomes-errors.test.ts`.

import {
  PROFILE_CODEC_ERROR_CODES,
  isBleErrorCode,
  isProfileCodecErrorCode,
  recoveryFor,
  type BleErrorCode,
} from '../src/index';
import { PROFILE_CODEC_ERROR_CODES as LIVE_ORACLE_CODEC_CODES } from '../../src/profiles/errors';

function cloneViaJson(value: unknown): unknown {
  return JSON.parse(JSON.stringify(value));
}

describe('profile codec identity catalog (0.1.2 additive amendment)', () => {
  test('freezes exactly the four oracle identities in oracle order', () => {
    expect([...PROFILE_CODEC_ERROR_CODES]).toEqual([...LIVE_ORACLE_CODEC_CODES]);
  });

  test('every identity resolves through the guard (wire-exact)', () => {
    for (const code of LIVE_ORACLE_CODEC_CODES) {
      expect(isProfileCodecErrorCode(code)).toBe(true);
    }
    for (const code of PROFILE_CODEC_ERROR_CODES) {
      expect(isProfileCodecErrorCode(code)).toBe(true);
      expect(LIVE_ORACLE_CODEC_CODES.includes(code)).toBe(true);
    }
  });

  test('codec identities stay disjoint from transport errors (LOW-2)', () => {
    // Codec failures carry no domain and no recovery disposition: they must
    // never resolve as `BleErrorCode` (which would route them through
    // `recoveryFor` and invent a disposition).
    for (const code of PROFILE_CODEC_ERROR_CODES) {
      expect(isBleErrorCode(code)).toBe(false);
      expect(recoveryFor(code as unknown as BleErrorCode)).toBeUndefined();
    }
    for (const code of LIVE_ORACLE_CODEC_CODES) {
      expect(isBleErrorCode(code)).toBe(false);
    }
  });

  test('rejects unknown codes, case drift, and non-strings', () => {
    expect(isProfileCodecErrorCode('profile.codec.unknown')).toBe(false);
    expect(isProfileCodecErrorCode('profile.codec')).toBe(false);
    expect(isProfileCodecErrorCode('Profile.Codec.Truncated')).toBe(false);
    expect(isProfileCodecErrorCode('')).toBe(false);
    expect(isProfileCodecErrorCode(42)).toBe(false);
    expect(isProfileCodecErrorCode(null)).toBe(false);
    expect(isProfileCodecErrorCode(undefined)).toBe(false);
    expect(isProfileCodecErrorCode({})).toBe(false);
    expect(isProfileCodecErrorCode([])).toBe(false);
    expect(isProfileCodecErrorCode(['profile.codec.truncated'])).toBe(false);
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
