import { observationMatchesScanQuery, normalizeScanObservation, normalizeScanQuery } from '../public/scan-query'
import type { ScanObservation, ScanQuery } from '../public/scan-query'
import type { BackendScanPlanner, ScanPlan, ScanPlanningContext } from '../backend-contract/scan-planning'
import type { NormalizedScanObservation, NormalizedScanQuery } from '../backend-contract/scan-query'

export const MAX_PLANNER_DIFFERENTIAL_SCENARIOS = 32

export interface PlannerDifferentialScenario {
  readonly id: string
  readonly query: ScanQuery
  readonly observation: ScanObservation
  readonly expectedMatch: boolean
}

export interface PlannerDifferentialNativeMatcher<NativeFilter> {
  (filter: NativeFilter, observation: NormalizedScanObservation): boolean
}

export interface PlannerDifferentialTckOptions<NativeFilter> {
  readonly planner: BackendScanPlanner<NativeFilter>
  readonly context: ScanPlanningContext
  readonly scenarios: readonly PlannerDifferentialScenario[]
  readonly normalizeQuery?: (query: ScanQuery) => NormalizedScanQuery
  readonly normalizeObservation?: (observation: ScanObservation) => NormalizedScanObservation
  readonly nativeAccepts: PlannerDifferentialNativeMatcher<NativeFilter>
}

export type PlannerDifferentialFactId =
  | 'planner-native-projection-is-safe-superset'
  | 'planner-residual-matcher-is-differentially-equivalent'
  | 'planner-diagnostics-are-bounded-and-payload-free'

export interface PlannerDifferentialFact {
  readonly id: PlannerDifferentialFactId
  readonly holds: boolean
  readonly detail: Readonly<Record<string, boolean | number>>
}

export interface PlannerDifferentialObservation {
  readonly id: string
  readonly expectedMatch: boolean
  readonly referenceMatch: boolean
  readonly nativeAccepted: boolean
  readonly residualMatch: boolean
  readonly optimizedMatch: boolean
}

export interface PlannerDifferentialTckReport {
  readonly scenarioCount: number
  readonly observations: readonly PlannerDifferentialObservation[]
  readonly facts: readonly PlannerDifferentialFact[]
}

/**
 * Runs a bounded, deterministic planner differential against the canonical
 * residual matcher. The native matcher is supplied by the host test because
 * this helper never invents platform filter semantics.
 */
export function runPlannerDifferentialTck<NativeFilter>(
  options: PlannerDifferentialTckOptions<NativeFilter>
): PlannerDifferentialTckReport {
  if (options.scenarios.length > MAX_PLANNER_DIFFERENTIAL_SCENARIOS) {
    throw new Error(
      `planner differential scenarios are bounded at ${String(MAX_PLANNER_DIFFERENTIAL_SCENARIOS)} scenarios`
    )
  }
  const normalizeQuery = options.normalizeQuery ?? normalizeScanQuery
  const normalizeObservation = options.normalizeObservation ?? normalizeScanObservation
  const scenarioIds = new Set<string>()
  const observations: PlannerDifferentialObservation[] = []
  const plans: ScanPlan[] = []

  for (const scenario of options.scenarios) {
    if (scenario.id.length === 0 || scenarioIds.has(scenario.id)) {
      throw new Error(`planner differential scenario IDs must be unique and non-empty: ${scenario.id}`)
    }
    scenarioIds.add(scenario.id)
    const query = normalizeQuery(scenario.query)
    const observation = normalizeObservation(scenario.observation)
    const plan = options.planner.plan(query, options.context)
    const referenceMatch = observationMatchesScanQuery(query, observation)
    const residualMatch = observationMatchesScanQuery(plan.residual.query, observation)
    const nativeAccepted = options.nativeAccepts(plan.nativeFilter, observation)
    plans.push(plan)
    observations.push(
      Object.freeze({
        id: scenario.id,
        expectedMatch: scenario.expectedMatch,
        referenceMatch,
        nativeAccepted,
        residualMatch,
        optimizedMatch: nativeAccepted && residualMatch
      })
    )
  }

  const nativeSupersetHolds = options.scenarios.every((scenario, index) => {
    const observation = observations[index]
    return observation !== undefined && (!scenario.expectedMatch || observation.nativeAccepted)
  })
  const residualEquivalenceHolds = options.scenarios.every((scenario, index) => {
    const observation = observations[index]
    return (
      observation !== undefined &&
      observation.referenceMatch === scenario.expectedMatch &&
      observation.residualMatch === observation.referenceMatch &&
      observation.optimizedMatch === scenario.expectedMatch
    )
  })
  const diagnosticsHolds = plans.every(plan => boundedPayloadFreeDiagnostics(plan))

  return Object.freeze({
    scenarioCount: options.scenarios.length,
    observations: Object.freeze(observations),
    facts: Object.freeze([
      Object.freeze({
        id: 'planner-native-projection-is-safe-superset',
        holds: nativeSupersetHolds,
        detail: Object.freeze({
          scenarioCount: options.scenarios.length,
          expectedMatches: options.scenarios.filter(scenario => scenario.expectedMatch).length,
          nativeRejections: observations.filter(observation => !observation.nativeAccepted).length
        })
      }),
      Object.freeze({
        id: 'planner-residual-matcher-is-differentially-equivalent',
        holds: residualEquivalenceHolds,
        detail: Object.freeze({
          scenarioCount: options.scenarios.length,
          residualMismatches: observations.filter(
            observation => observation.residualMatch !== observation.referenceMatch
          ).length
        })
      }),
      Object.freeze({
        id: 'planner-diagnostics-are-bounded-and-payload-free',
        holds: diagnosticsHolds,
        detail: Object.freeze({
          scenarioCount: options.scenarios.length,
          maximumNativePredicates: Math.max(0, ...plans.map(plan => plan.native.predicates.length)),
          maximumLimitations: Math.max(0, ...plans.map(plan => plan.limitations.length))
        })
      })
    ])
  })
}

function boundedPayloadFreeDiagnostics(plan: ScanPlan): boolean {
  if (
    plan.native.predicates.length > 64 ||
    plan.residual.predicates.length > 64 ||
    plan.unavailable.length > 64 ||
    plan.limitations.length > 32
  ) {
    return false
  }
  const diagnostics = JSON.stringify({
    native: plan.native,
    residual: { predicates: plan.residual.predicates, complete: plan.residual.complete },
    unavailable: plan.unavailable,
    limitations: plan.limitations
  })
  return !diagnostics.includes('Heart Strap') && !diagnostics.includes('010203')
}
