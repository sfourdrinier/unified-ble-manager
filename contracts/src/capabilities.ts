// contracts/src/capabilities.ts — C-UBM DRAFT (pending U1 acceptance).
//
// Typed capability descriptors with runtime authority. Derived from
// docs/UNIFIED_SEMANTICS.md §16 and src/backend-contract/capabilities.ts
// (read-only reference). Capability data is runtime information from the
// instantiated backend, never a static platform matrix.

import { freezeTable } from './freeze';
import { contractError } from './outcomes';

export type CapabilityState = 'supported' | 'limited' | 'unsupported' | 'unavailable';

export const CAPABILITY_STATES: readonly CapabilityState[] = freezeTable([
  'supported',
  'limited',
  'unsupported',
  'unavailable',
] satisfies readonly CapabilityState[]);

export interface Limitation {
  readonly code: string;
  readonly explanation: string;
  readonly affectedGuarantee: string;
}

export type EvidenceLevel =
  | 'blocked'
  | 'deterministic'
  | 'live-preview'
  | 'supported'
  | 'reliability-qualified';

export interface EvidenceReceipt {
  readonly receiptId: string;
  readonly evidenceLevel: EvidenceLevel;
  readonly implementationVersion: string;
  readonly sourceDigest: string;
  readonly scenarioIds: readonly string[];
}

export interface CapabilityDescriptor {
  readonly id: string;
  readonly state: CapabilityState;
  readonly limits: { readonly [key: string]: number };
  readonly limitations: readonly Limitation[];
  readonly evidence: EvidenceReceipt;
}

function requireLimitation(input: unknown, operation: string): Limitation {
  if (typeof input !== 'object' || input === null || Array.isArray(input)) {
    throw contractError('argument.invalid', 'capability', operation);
  }
  const candidate: { code?: unknown; explanation?: unknown; affectedGuarantee?: unknown } = input;
  if (
    typeof candidate.code !== 'string' ||
    candidate.code.length === 0 ||
    typeof candidate.explanation !== 'string' ||
    candidate.explanation.length === 0 ||
    typeof candidate.affectedGuarantee !== 'string' ||
    candidate.affectedGuarantee.length === 0
  ) {
    throw contractError('argument.invalid', 'capability', operation);
  }
  return Object.freeze({
    code: candidate.code,
    explanation: candidate.explanation,
    affectedGuarantee: candidate.affectedGuarantee,
  });
}

function asEvidenceLevel(value: unknown): EvidenceLevel {
  if (typeof value !== 'string') {
    throw contractError('argument.invalid', 'capability', 'capability.evidence.level');
  }
  switch (value) {
    case 'blocked':
      return 'blocked';
    case 'deterministic':
      return 'deterministic';
    case 'live-preview':
      return 'live-preview';
    case 'supported':
      return 'supported';
    case 'reliability-qualified':
      return 'reliability-qualified';
    default:
      throw contractError('argument.invalid', 'capability', 'capability.evidence.level');
  }
}

export function makeCapabilityDescriptor(input: {
  readonly id: unknown;
  readonly state: unknown;
  readonly limits: { readonly [key: string]: number };
  readonly limitations: readonly unknown[];
  readonly evidence: {
    readonly receiptId: unknown;
    readonly evidenceLevel: unknown;
    readonly implementationVersion: unknown;
    readonly sourceDigest: unknown;
    readonly scenarioIds: readonly unknown[];
  };
}): CapabilityDescriptor {
  if (typeof input.id !== 'string' || input.id.length === 0) {
    throw contractError('argument.invalid', 'capability', 'capability.id');
  }
  if (typeof input.state !== 'string' || !CAPABILITY_STATES.some(state => state === input.state)) {
    throw contractError('argument.invalid', 'capability', 'capability.state');
  }
  let state: CapabilityState;
  switch (input.state) {
    case 'supported':
      state = 'supported';
      break;
    case 'limited':
      state = 'limited';
      break;
    case 'unsupported':
      state = 'unsupported';
      break;
    case 'unavailable':
      state = 'unavailable';
      break;
    default:
      throw contractError('argument.invalid', 'capability', 'capability.state');
  }
  const limitations: Limitation[] = [];
  for (const entry of input.limitations) {
    limitations.push(requireLimitation(entry, 'capability.limitation'));
  }
  if (state !== 'supported' && limitations.length === 0) {
    throw contractError('argument.invalid', 'capability', 'capability.reason-required');
  }
  if (
    typeof input.evidence.receiptId !== 'string' ||
    input.evidence.receiptId.length === 0 ||
    typeof input.evidence.implementationVersion !== 'string' ||
    input.evidence.implementationVersion.length === 0 ||
    typeof input.evidence.sourceDigest !== 'string' ||
    input.evidence.sourceDigest.length === 0
  ) {
    throw contractError('argument.invalid', 'capability', 'capability.evidence');
  }
  const evidenceLevel = asEvidenceLevel(input.evidence.evidenceLevel);
  for (const key of Object.keys(input.limits)) {
    const bound = input.limits[key];
    if (typeof bound !== 'number' || !Number.isFinite(bound) || bound < 0) {
      throw contractError('argument.invalid', 'capability', 'capability.limits');
    }
  }
  const scenarioIds: string[] = [];
  for (const id of input.evidence.scenarioIds) {
    if (typeof id !== 'string' || id.length === 0) {
      throw contractError('argument.invalid', 'capability', 'capability.evidence.scenario');
    }
    scenarioIds.push(id);
  }
  return Object.freeze({
    id: input.id,
    state,
    limits: Object.freeze({ ...input.limits }),
    limitations: Object.freeze(limitations),
    evidence: Object.freeze({
      receiptId: input.evidence.receiptId,
      evidenceLevel,
      implementationVersion: input.evidence.implementationVersion,
      sourceDigest: input.evidence.sourceDigest,
      scenarioIds: Object.freeze(scenarioIds),
    }),
  });
}

export type CapabilityAdmission = 'proceed' | 'proceed-with-limitation';

// A platform that cannot answer says so with a typed rejection and never
// substitutes something plausible.
export function assertCapabilityAllows(
  descriptor: CapabilityDescriptor,
  operation: string,
): CapabilityAdmission {
  switch (descriptor.state) {
    case 'supported':
      return 'proceed';
    case 'limited':
      return 'proceed-with-limitation';
    case 'unsupported':
      throw contractError('capability.unsupported', 'capability', operation);
    case 'unavailable':
      throw contractError('capability.unavailable', 'capability', operation);
  }
}

export const BUILT_IN_CAPABILITY_IDS: readonly string[] = freezeTable([
  'central.scan',
  'central.connect',
  'central.discover',
  'central.read',
  'central.write',
  'central.subscribe',
  'connection:rssi',
  'connection:effective-mtu',
  'connection:request-mtu',
  'connection:priority',
  'connection:parameters',
  'connection:phy',
  'connection:subrate',
  'gatt:maximum-write-length',
  'gatt:write-without-response-readiness',
  'peripheral.advertise',
  'peripheral.respond',
  'peripheral.notify',
] satisfies readonly string[]);

export function isBuiltInCapabilityId(value: unknown): value is string {
  return typeof value === 'string' && BUILT_IN_CAPABILITY_IDS.some(id => id === value);
}
