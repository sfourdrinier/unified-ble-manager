// src/tck/index.ts

export { runBackendTck } from './runner'
export { baseTckScenarios, findTckScenario } from './scenarios'
export { TckAssertionError } from './contracts'
export { runPlannerDifferentialTck, MAX_PLANNER_DIFFERENTIAL_SCENARIOS } from './planner-differential'
export type {
  BackendTckFactory,
  BackendTckFixture,
  RegisteredFeature,
  TckControllerAction,
  TckFact,
  TckFactId,
  TckFeatureBinding,
  TckFeatureSuite,
  TckProofLabel,
  TckProofScope,
  TckRuntimeIdentity,
  TckRunOptions,
  TckRunReport,
  TckScenarioDefinition,
  TckScenarioController,
  TckScenarioId,
  TckScenarioReceipt
} from './contracts'
export type {
  PlannerDifferentialFact,
  PlannerDifferentialFactId,
  PlannerDifferentialNativeMatcher,
  PlannerDifferentialObservation,
  PlannerDifferentialScenario,
  PlannerDifferentialTckOptions,
  PlannerDifferentialTckReport
} from './planner-differential'
