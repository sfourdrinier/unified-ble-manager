import { snapshotNormalizedScanQuery } from './scan-query'
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

export interface ScanPlanResidualProjection extends ScanPlanProjection {
  readonly query: NormalizedScanQuery
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

export interface ScanPlan {
  readonly sourceQuery: NormalizedScanQuery
  readonly queryDigest: string
  readonly residualQueryDigest: string
  readonly nativeGuarantee: 'exact' | 'safe-superset'
  readonly native: ScanPlanProjection
  readonly residual: ScanPlanResidualProjection
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

export interface BackendScanExecutionPlan<NativeFilter> extends ScanPlan {
  readonly nativeFilter: NativeFilter
}

export interface BackendScanPlanner<NativeFilter, Context extends ScanPlanningContext = ScanPlanningContext> {
  plan(query: NormalizedScanQuery, context: Context): BackendScanExecutionPlan<NativeFilter>
}

/** Describes every normalized predicate without evaluating it. */
export function describeScanPredicates(query: NormalizedScanQuery): readonly ScanPredicateDescription[] {
  const descriptions: ScanPredicateDescription[] = []
  for (const [clauseSet, clauses] of [
    ['anyOf', query.anyOf],
    ['exclude', query.exclude]
  ] as const) {
    if (clauses === null) continue
    clauses.forEach((clause, clauseIndex) => {
      if (clause.peers !== null) descriptions.push({ clauseSet, clauseIndex, field: 'peers', operator: 'equals' })
      if (clause.services !== null) {
        if (clause.services.any.length > 0)
          descriptions.push({ clauseSet, clauseIndex, field: 'services', operator: 'any' })
        if (clause.services.all.length > 0)
          descriptions.push({ clauseSet, clauseIndex, field: 'services', operator: 'all' })
      }
      if (clause.names !== null) {
        if (clause.names.exact.length > 0)
          descriptions.push({ clauseSet, clauseIndex, field: 'names', operator: 'exact' })
        if (clause.names.prefixes.length > 0)
          descriptions.push({ clauseSet, clauseIndex, field: 'names', operator: 'prefixes' })
      }
      if (clause.manufacturerData !== null) {
        if (clause.manufacturerData.any.length > 0)
          descriptions.push({ clauseSet, clauseIndex, field: 'manufacturerData', operator: 'any' })
        if (clause.manufacturerData.all.length > 0)
          descriptions.push({ clauseSet, clauseIndex, field: 'manufacturerData', operator: 'all' })
      }
      if (clause.serviceData !== null) {
        if (clause.serviceData.any.length > 0)
          descriptions.push({ clauseSet, clauseIndex, field: 'serviceData', operator: 'any' })
        if (clause.serviceData.all.length > 0)
          descriptions.push({ clauseSet, clauseIndex, field: 'serviceData', operator: 'all' })
      }
      if (clause.rssi !== null) {
        if (clause.rssi.minimum !== undefined)
          descriptions.push({ clauseSet, clauseIndex, field: 'rssi', operator: 'minimum' })
        if (clause.rssi.maximum !== undefined)
          descriptions.push({ clauseSet, clauseIndex, field: 'rssi', operator: 'maximum' })
      }
      if (clause.connectable !== undefined)
        descriptions.push({ clauseSet, clauseIndex, field: 'connectable', operator: 'equals' })
    })
  }
  return Object.freeze(
    descriptions.sort((left, right) => compareCanonical(JSON.stringify(left), JSON.stringify(right)))
  )
}

const MAX_PROJECTION_PREDICATES = 64
const MAX_UNAVAILABLE_PREDICATES = 64
const MAX_LIMITATIONS = 32

/**
 * Snapshots plan diagnostics without changing the canonical residual query. The native
 * projection is descriptive only; this helper does not evaluate any predicate.
 */
export function snapshotScanPlan(plan: ScanPlan): ScanPlan {
  return snapshotPlanFields(plan)
}

export function snapshotScanExecutionPlan<NativeFilter>(
  plan: BackendScanExecutionPlan<NativeFilter>,
  snapshotNativeFilter: (nativeFilter: NativeFilter) => NativeFilter
): BackendScanExecutionPlan<NativeFilter> {
  assertExactKeys(
    plan,
    [
      'sourceQuery',
      'queryDigest',
      'residualQueryDigest',
      'nativeGuarantee',
      'native',
      'residual',
      'unavailable',
      'limitations',
      'estimatedCost',
      'nativeFilter'
    ],
    'scan execution plan'
  )
  const snapshot = snapshotPlanFields({
    sourceQuery: plan.sourceQuery,
    queryDigest: plan.queryDigest,
    residualQueryDigest: plan.residualQueryDigest,
    nativeGuarantee: plan.nativeGuarantee,
    native: plan.native,
    residual: plan.residual,
    unavailable: plan.unavailable,
    limitations: plan.limitations,
    estimatedCost: plan.estimatedCost
  })
  const nativeFilter = snapshotNativeFilter(plan.nativeFilter)
  if (
    nativeFilter === plan.nativeFilter &&
    ((typeof nativeFilter === 'object' && nativeFilter !== null) || typeof nativeFilter === 'function')
  ) {
    throw new Error('defensive native-filter snapshot must not preserve identity')
  }
  return Object.freeze({ ...snapshot, nativeFilter })
}

function snapshotPlanFields(plan: ScanPlan): ScanPlan {
  assertExactKeys(
    plan,
    [
      'queryDigest',
      'sourceQuery',
      'residualQueryDigest',
      'nativeGuarantee',
      'native',
      'residual',
      'unavailable',
      'limitations',
      'estimatedCost'
    ],
    'scan plan'
  )
  const sourceQuery = snapshotNormalizedScanQuery(plan.sourceQuery)
  const residualQuery = snapshotNormalizedScanQuery(plan.residual.query)
  if (plan.queryDigest !== sourceQuery.digest) {
    throw new Error('scan plan source query digest must match queryDigest')
  }
  if (plan.residualQueryDigest !== residualQuery.digest) {
    throw new Error('scan plan residual query digest must match residualQueryDigest')
  }
  assertNativeGuarantee(plan.nativeGuarantee)
  if (
    plan.nativeGuarantee === 'exact' &&
    (!plan.native.complete ||
      !plan.residual.complete ||
      plan.residual.predicates.length > 0 ||
      !isMatchAllQuery(plan.residual.query))
  ) {
    throw new Error('exact native projection must be complete for the whole query')
  }
  if (plan.nativeGuarantee === 'safe-superset' && !plan.residual.complete) {
    throw new Error('safe-superset plan requires a complete residual')
  }
  if (plan.nativeGuarantee === 'safe-superset' && plan.queryDigest !== plan.residualQueryDigest) {
    throw new Error('safe-superset plan must retain the source query as residual')
  }
  assertEstimatedCost(plan.estimatedCost)
  const native = snapshotProjection(plan.native, 'native', sourceQuery)
  const residual = Object.freeze({
    ...snapshotProjection(plan.residual, 'residual', residualQuery),
    query: residualQuery
  })
  const unavailable = snapshotPredicates(plan.unavailable, 'unavailable', MAX_UNAVAILABLE_PREDICATES, sourceQuery)
  const limitations = snapshotLimitations(plan.limitations, sourceQuery)
  const snapshot = {
    sourceQuery,
    queryDigest: plan.queryDigest,
    residualQueryDigest: plan.residualQueryDigest,
    nativeGuarantee: plan.nativeGuarantee,
    native,
    residual,
    unavailable,
    limitations,
    estimatedCost: plan.estimatedCost
  }
  return Object.freeze(snapshot)
}

function isMatchAllQuery(query: NormalizedScanQuery): boolean {
  return query.anyOf === null && query.exclude === null
}

function snapshotProjection(
  projection: ScanPlanProjection,
  name: string,
  query: NormalizedScanQuery
): ScanPlanProjection {
  assertExactKeys(
    projection,
    name === 'residual' ? ['predicates', 'complete', 'query'] : ['predicates', 'complete'],
    name
  )
  if (typeof projection.complete !== 'boolean') {
    throw new Error(`scan plan ${name} contains an invalid completeness value`)
  }
  return Object.freeze({
    predicates: snapshotPredicates(projection.predicates, `${name}.predicates`, MAX_PROJECTION_PREDICATES, query),
    complete: projection.complete
  })
}

function snapshotPredicates(
  predicates: readonly ScanPredicateDescription[],
  name: string,
  maximum: number,
  query: NormalizedScanQuery
): readonly ScanPredicateDescription[] {
  if (predicates.length > maximum) throw new Error(`scan plan ${name} exceeds bounded predicate count`)
  const snapshot = predicates.map(predicate => {
    assertExactKeys(predicate, ['clauseSet', 'clauseIndex', 'field', 'operator'], `${name}.predicate`)
    if (!Number.isSafeInteger(predicate.clauseIndex) || predicate.clauseIndex < 0) {
      throw new Error(`scan plan ${name} contains an invalid clause index`)
    }
    if (!isPredicateClauseSet(predicate.clauseSet)) {
      throw new Error(`scan plan ${name} contains an invalid predicate clause set`)
    }
    if (!isPredicateField(predicate.field) || !isPredicateOperator(predicate.operator)) {
      throw new Error(`scan plan ${name} contains an invalid predicate`)
    }
    assertPredicateReference(predicate, query, name)
    return Object.freeze({
      clauseSet: predicate.clauseSet,
      clauseIndex: predicate.clauseIndex,
      field: predicate.field,
      operator: predicate.operator
    })
  })
  return Object.freeze(snapshot.sort((left, right) => compareCanonical(predicateKey(left), predicateKey(right))))
}

function snapshotLimitations(
  limitations: readonly ScanPlanLimitation[],
  query: NormalizedScanQuery
): readonly ScanPlanLimitation[] {
  if (limitations.length > MAX_LIMITATIONS) throw new Error('scan plan exceeds bounded limitation count')
  const snapshot = limitations.map(limitation => {
    assertExactKeys(limitation, ['code', 'predicate', 'explanation', 'effect'], 'scan plan limitation')
    assertLimitationCode(limitation.code)
    const predicate = snapshotPredicates([limitation.predicate], 'limitation.predicate', 1, query)[0]
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

function assertPredicateReference(predicate: ScanPredicateDescription, query: NormalizedScanQuery, name: string): void {
  const clauses = predicate.clauseSet === 'anyOf' ? query.anyOf : query.exclude
  const clause = clauses?.[predicate.clauseIndex]
  if (clause === undefined) throw new Error(`scan plan ${name} contains an out-of-range clause index`)
  const supported =
    (predicate.field === 'peers' && predicate.operator === 'equals' && clause.peers !== null) ||
    (predicate.field === 'services' &&
      clause.services !== null &&
      (predicate.operator === 'any' || predicate.operator === 'all')) ||
    (predicate.field === 'names' &&
      clause.names !== null &&
      (predicate.operator === 'exact' || predicate.operator === 'prefixes')) ||
    (predicate.field === 'manufacturerData' &&
      clause.manufacturerData !== null &&
      (predicate.operator === 'any' || predicate.operator === 'all')) ||
    (predicate.field === 'serviceData' &&
      clause.serviceData !== null &&
      (predicate.operator === 'any' || predicate.operator === 'all')) ||
    (predicate.field === 'rssi' &&
      clause.rssi !== null &&
      (predicate.operator === 'minimum' || predicate.operator === 'maximum')) ||
    (predicate.field === 'connectable' && clause.connectable !== undefined && predicate.operator === 'equals')
  if (!supported) throw new Error(`scan plan ${name} contains an unrelated predicate reference`)
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

function assertExactKeys(value: object, keys: readonly string[], name: string): void {
  const actual = Object.keys(value).sort()
  const expected = [...keys].sort()
  if (actual.length !== expected.length || actual.some((key, index) => key !== expected[index])) {
    throw new Error(`${name} contains an unknown key`)
  }
}
