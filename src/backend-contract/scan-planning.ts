import type { NormalizedScanQuery } from './scan-query'

export type ScanPredicateClauseSet = 'anyOf' | 'exclude'
export type ScanPredicateField =
  | 'peers'
  | 'services'
  | 'names'
  | 'manufacturerData'
  | 'serviceData'
  | 'rssi'
  | 'connectable'
export type ScanPredicateOperator = 'any' | 'all' | 'exact' | 'prefixes' | 'minimum' | 'maximum' | 'equals'

/** A bounded, payload-free description of one normalized query predicate. */
export interface ScanPredicateDescription {
  readonly clauseSet: ScanPredicateClauseSet
  readonly clauseIndex: number
  readonly field: ScanPredicateField
  readonly operator: ScanPredicateOperator
}

/** The predicates assigned to a native or residual projection. */
export interface ScanPlanProjection {
  readonly predicates: readonly ScanPredicateDescription[]
  /** True only when this projection exactly represents its assigned semantics. */
  readonly complete: boolean
}

/** The canonical PR4 query retained for the one and only residual matcher. */
export type ScanPlanningNormalizedQuery = NormalizedScanQuery

export interface ScanPlanResidualProjection<
  NormalizedQuery extends ScanPlanningNormalizedQuery = ScanPlanningNormalizedQuery
> extends ScanPlanProjection {
  readonly query: NormalizedQuery
}

export type ScanPlanLimitationCode =
  | 'native-filter-incomplete'
  | 'native-predicate-unavailable'
  | 'observation-field-unavailable'
  | 'host-predicate-restricted'

export type ScanPlanLimitationExplanation =
  | 'predicate remains in the canonical residual matcher'
  | 'required observation field is unavailable on this host'
  | 'host restriction prevents native evaluation'
  | 'native filter is a safe superset'

export interface ScanPlanLimitation {
  readonly code: ScanPlanLimitationCode
  readonly predicate: ScanPredicateDescription
  readonly explanation: ScanPlanLimitationExplanation
  readonly effect: 'performance-only' | 'field-unavailable' | 'host-restriction'
}

export interface ScanPlan<NormalizedQuery extends ScanPlanningNormalizedQuery = ScanPlanningNormalizedQuery> {
  readonly queryDigest: string
  readonly nativeGuarantee: 'exact' | 'safe-superset'
  readonly native: ScanPlanProjection
  readonly residual: ScanPlanResidualProjection<NormalizedQuery>
  readonly unavailable: readonly ScanPredicateDescription[]
  readonly limitations: readonly ScanPlanLimitation[]
  readonly estimatedCost: 'native-only' | 'low' | 'moderate' | 'high'
}

export type ScanObservationField =
  | 'peerReference'
  | 'localName'
  | 'rssi'
  | 'connectable'
  | 'serviceUuids'
  | 'manufacturerData'
  | 'serviceData'

/** Context is intentionally host-neutral; platform-specific fields belong to later planner slices. */
export interface ScanPlanningContext {
  readonly backendId: string
  readonly platformId: string
  readonly availableObservationFields: readonly ScanObservationField[]
}

export interface BackendScanExecutionPlan<
  NativeFilter,
  NormalizedQuery extends ScanPlanningNormalizedQuery = ScanPlanningNormalizedQuery
> extends ScanPlan<NormalizedQuery> {
  readonly nativeFilter: NativeFilter
}

export interface BackendScanPlanner<
  NativeFilter,
  NormalizedQuery extends ScanPlanningNormalizedQuery = ScanPlanningNormalizedQuery,
  Context extends ScanPlanningContext = ScanPlanningContext
> {
  plan(query: NormalizedQuery, context: Context): BackendScanExecutionPlan<NativeFilter, NormalizedQuery>
}

const MAX_PROJECTION_PREDICATES = 64
const MAX_UNAVAILABLE_PREDICATES = 64
const MAX_LIMITATIONS = 32

/**
 * Snapshots plan diagnostics without changing the canonical residual query. The native
 * projection is descriptive only; this helper does not evaluate any predicate.
 */
export function snapshotScanPlan<NormalizedQuery extends ScanPlanningNormalizedQuery>(
  plan: ScanPlan<NormalizedQuery>
): ScanPlan<NormalizedQuery> {
  assertNormalizedScanQuery(plan.residual.query)
  if (plan.queryDigest !== plan.residual.query.digest) {
    throw new Error('scan plan residual query digest must match queryDigest')
  }
  assertNativeGuarantee(plan.nativeGuarantee)
  assertEstimatedCost(plan.estimatedCost)
  const native = snapshotProjection(plan.native, 'native')
  const residual = Object.freeze({
    ...snapshotProjection(plan.residual, 'residual'),
    query: plan.residual.query
  })
  const unavailable = snapshotPredicates(plan.unavailable, 'unavailable', MAX_UNAVAILABLE_PREDICATES)
  const limitations = snapshotLimitations(plan.limitations)
  return Object.freeze({
    ...plan,
    native,
    residual,
    unavailable,
    limitations
  })
}

function snapshotProjection(projection: ScanPlanProjection, name: string): ScanPlanProjection {
  if (typeof projection.complete !== 'boolean') {
    throw new Error(`scan plan ${name} contains an invalid completeness value`)
  }
  return Object.freeze({
    predicates: snapshotPredicates(projection.predicates, `${name}.predicates`, MAX_PROJECTION_PREDICATES),
    complete: projection.complete
  })
}

function snapshotPredicates(
  predicates: readonly ScanPredicateDescription[],
  name: string,
  maximum: number
): readonly ScanPredicateDescription[] {
  if (predicates.length > maximum) throw new Error(`scan plan ${name} exceeds bounded predicate count`)
  const snapshot = predicates.map(predicate => {
    if (!Number.isSafeInteger(predicate.clauseIndex) || predicate.clauseIndex < 0) {
      throw new Error(`scan plan ${name} contains an invalid clause index`)
    }
    if (!isPredicateClauseSet(predicate.clauseSet)) {
      throw new Error(`scan plan ${name} contains an invalid predicate clause set`)
    }
    if (!isPredicateField(predicate.field) || !isPredicateOperator(predicate.operator)) {
      throw new Error(`scan plan ${name} contains an invalid predicate`)
    }
    return Object.freeze({
      clauseSet: predicate.clauseSet,
      clauseIndex: predicate.clauseIndex,
      field: predicate.field,
      operator: predicate.operator
    })
  })
  return Object.freeze(snapshot.sort((left, right) => compareCanonical(predicateKey(left), predicateKey(right))))
}

function snapshotLimitations(limitations: readonly ScanPlanLimitation[]): readonly ScanPlanLimitation[] {
  if (limitations.length > MAX_LIMITATIONS) throw new Error('scan plan exceeds bounded limitation count')
  const snapshot = limitations.map(limitation => {
    assertLimitationCode(limitation.code)
    const predicate = snapshotPredicates([limitation.predicate], 'limitation.predicate', 1)[0]
    if (predicate === undefined) throw new Error('scan plan limitation predicate is missing')
    assertLimitationExplanation(limitation.explanation)
    assertLimitationEffect(limitation.effect)
    return Object.freeze({
      code: limitation.code,
      predicate,
      explanation: limitation.explanation,
      effect: limitation.effect
    })
  })
  return Object.freeze(snapshot.sort((left, right) => compareCanonical(JSON.stringify(left), JSON.stringify(right))))
}

function assertLimitationCode(value: ScanPlanLimitationCode): void {
  if (
    value !== 'native-filter-incomplete' &&
    value !== 'native-predicate-unavailable' &&
    value !== 'observation-field-unavailable' &&
    value !== 'host-predicate-restricted'
  ) {
    throw new Error('scan plan limitation code is invalid')
  }
}

function assertLimitationExplanation(value: ScanPlanLimitationExplanation): void {
  if (
    value !== 'predicate remains in the canonical residual matcher' &&
    value !== 'required observation field is unavailable on this host' &&
    value !== 'host restriction prevents native evaluation' &&
    value !== 'native filter is a safe superset'
  ) {
    throw new Error('scan plan limitation explanation is invalid')
  }
}

function assertLimitationEffect(value: ScanPlanLimitation['effect']): void {
  if (value !== 'performance-only' && value !== 'field-unavailable' && value !== 'host-restriction') {
    throw new Error('scan plan limitation effect is invalid')
  }
}

function assertNormalizedScanQuery(query: ScanPlanningNormalizedQuery): void {
  if (
    typeof query.digest !== 'string' ||
    query.digest.length === 0 ||
    (query.anyOf !== null && !Array.isArray(query.anyOf)) ||
    (query.exclude !== null && !Array.isArray(query.exclude)) ||
    !Object.isFrozen(query) ||
    (query.anyOf !== null && !Object.isFrozen(query.anyOf)) ||
    (query.exclude !== null && !Object.isFrozen(query.exclude))
  ) {
    throw new Error('scan plan residual query must be a frozen normalized query')
  }
}

function assertNativeGuarantee(value: ScanPlan['nativeGuarantee']): void {
  if (value !== 'exact' && value !== 'safe-superset') {
    throw new Error('scan plan contains an invalid native guarantee')
  }
}

function assertEstimatedCost(value: ScanPlan['estimatedCost']): void {
  if (value !== 'native-only' && value !== 'low' && value !== 'moderate' && value !== 'high') {
    throw new Error('scan plan contains an invalid estimated cost')
  }
}

function isPredicateClauseSet(value: ScanPredicateDescription['clauseSet']): value is ScanPredicateClauseSet {
  return value === 'anyOf' || value === 'exclude'
}

function isPredicateField(value: ScanPredicateDescription['field']): value is ScanPredicateField {
  return (
    value === 'peers' ||
    value === 'services' ||
    value === 'names' ||
    value === 'manufacturerData' ||
    value === 'serviceData' ||
    value === 'rssi' ||
    value === 'connectable'
  )
}

function isPredicateOperator(value: ScanPredicateDescription['operator']): value is ScanPredicateOperator {
  return (
    value === 'any' ||
    value === 'all' ||
    value === 'exact' ||
    value === 'prefixes' ||
    value === 'minimum' ||
    value === 'maximum' ||
    value === 'equals'
  )
}

function predicateKey(predicate: ScanPredicateDescription): string {
  return JSON.stringify(predicate)
}

function compareCanonical(left: string, right: string): number {
  return left < right ? -1 : left > right ? 1 : 0
}
