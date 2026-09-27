// src/backends/reactnative/react-native-continuation.ts
//
// `background.continuation` runtime capabilities for the React Native
// Rust-core backend (BGS4). One capability per wake strategy, reported by
// the instantiated backend — never a static platform matrix.
//
// - `background:wake-on-appearance`: the OS wakes the dead process on peer
//   appearance (Android CDM API 31+ for an armed associated peer; Apple
//   relaunch into `willRestoreState` on BLE events).
// - `background:native-resubscribe`: the wake reconnects the declared known
//   peer and resubscribes the declared characteristics through the Rust core
//   with no JavaScript, through the shared native continuation owner.
// - Android task/service mechanisms require the running native binding and
//   presence API. Their availability is not proof of dispatch or app completion.

import {
  BACKGROUND_CONTINUATION_FEATURE_IDS,
  type BackgroundContinuationFeatureId
} from '../../backend-contract/background-continuation'
import {
  BUILT_IN_FEATURE_IDS,
  createFeatureRegistry,
  type FeatureRegistry,
  type Limitation
} from '../../backend-contract/capabilities'
import { contractError } from '../../backend-contract/errors'
import { version, versionRange, type SerializableRecord } from '../../backend-contract/primitives'

export type ReactNativeContinuationPlatform = 'android' | 'apple'

/** Facts about the running OS the host supplies (never inferred). */
export interface ReactNativeContinuationRuntimeFacts {
  readonly continuationBindingAvailable?: boolean
  /** Android API level (`Platform.Version`); CDM presence needs API 31+. */
  readonly androidApiLevel: number | null
  /** The running Apple host supplied a configured native restoration authority. */
  readonly appleRestorationConfigured?: boolean
}

/** The first Android API level with CDM device-presence observation. */
export const ANDROID_PRESENCE_API_LEVEL = 31

// Marker registrations bind the catalog scenario like every other
// deterministic registration. Native execution is separately exercised by the
// Rust, JNI and Swift continuation suites; this marker is not hardware evidence.
const scenarioIds = Object.freeze(['capability.truth-limits-evidence-and-binding'])
const schemaRange = versionRange(version('capability-schema', 1), version('capability-schema', 1))

function markerImplementation(operation: string) {
  return Object.freeze({
    async invoke(_input: SerializableRecord): Promise<SerializableRecord> {
      throw contractError('lifecycle.invalid-state', 'restoration', operation)
    }
  })
}

function limitedRegistration(
  id: BackgroundContinuationFeatureId,
  platform: string,
  implementationVersion: string,
  limitation: Limitation
) {
  const sourceDigest = `react-native-${platform}-${id.replace(':', '-')}-v1`
  const frozen = Object.freeze(limitation)
  return Object.freeze({
    id,
    state: 'limited' as const,
    selectedSchemaRange: schemaRange,
    implementationOrigin: 'backend-native' as const,
    implementation: markerImplementation(`${id}.invoke-without-wake`),
    tck: Object.freeze({
      suiteId: 'capability.catalog-v2',
      requiredScenarioIds: scenarioIds,
      contractRange: schemaRange
    }),
    evidence: Object.freeze({
      receiptId: `${sourceDigest}:deterministic`,
      evidenceLevel: 'deterministic' as const,
      implementationVersion,
      sourceDigest,
      scenarioIds,
      limitations: Object.freeze([frozen])
    }),
    limitations: Object.freeze([frozen]),
    limits: Object.freeze({
      declaredPeers: Object.freeze({ maximum: 64, minimum: null, unit: 'peers' }),
      declaredResubscriptions: Object.freeze({ maximum: 64, minimum: null, unit: 'characteristics' })
    })
  })
}

function unsupportedRegistration(
  id: BackgroundContinuationFeatureId,
  platform: string,
  implementationVersion: string,
  limitation: Limitation
) {
  const sourceDigest = `react-native-${platform}-${id.replace(':', '-')}-v1`
  const frozen = Object.freeze(limitation)
  return Object.freeze({
    id,
    state: 'unsupported' as const,
    selectedSchemaRange: schemaRange,
    implementationOrigin: 'backend-native' as const,
    implementation: markerImplementation(`${id}.invoke-without-wake`),
    tck: Object.freeze({
      suiteId: 'capability.catalog-v2',
      requiredScenarioIds: scenarioIds,
      contractRange: schemaRange
    }),
    evidence: Object.freeze({
      receiptId: `${sourceDigest}:blocked`,
      evidenceLevel: 'blocked' as const,
      implementationVersion,
      sourceDigest,
      scenarioIds,
      limitations: Object.freeze([frozen])
    }),
    limitations: Object.freeze([frozen]),
    limits: Object.freeze({
      declaredPeers: Object.freeze({ maximum: 0, minimum: null, unit: 'peers' }),
      declaredResubscriptions: Object.freeze({ maximum: 0, minimum: null, unit: 'characteristics' })
    })
  })
}

/** Registers the per-strategy continuation capabilities for one platform and runtime. */
export function createReactNativeContinuationFeatureRegistry(
  platform: ReactNativeContinuationPlatform,
  implementationVersion: string,
  facts: ReactNativeContinuationRuntimeFacts
): FeatureRegistry {
  const ids: Record<keyof typeof BACKGROUND_CONTINUATION_FEATURE_IDS, BackgroundContinuationFeatureId> = {
    wakeOnAppearance: BUILT_IN_FEATURE_IDS.backgroundWakeOnAppearance,
    nativeResubscribe: BUILT_IN_FEATURE_IDS.backgroundNativeResubscribe,
    headlessTask: BUILT_IN_FEATURE_IDS.backgroundHeadlessTask,
    wakeNotification: BUILT_IN_FEATURE_IDS.backgroundWakeNotification
  }
  if (
    ids.wakeOnAppearance !== BACKGROUND_CONTINUATION_FEATURE_IDS.wakeOnAppearance ||
    ids.nativeResubscribe !== BACKGROUND_CONTINUATION_FEATURE_IDS.nativeResubscribe ||
    ids.headlessTask !== BACKGROUND_CONTINUATION_FEATURE_IDS.headlessTask ||
    ids.wakeNotification !== BACKGROUND_CONTINUATION_FEATURE_IDS.wakeNotification
  ) {
    throw contractError('lifecycle.invariant-violation', 'restoration', 'react-native-continuation.catalog-drift')
  }
  const androidWake =
    facts.androidApiLevel !== null &&
    Number.isSafeInteger(facts.androidApiLevel) &&
    facts.androidApiLevel >= ANDROID_PRESENCE_API_LEVEL
  const bindingAvailable = facts.continuationBindingAvailable === true
  const unavailableBinding: Limitation = {
    code: 'native-continuation-binding-required',
    explanation: 'The running native binding does not expose declaration, status and claim ownership methods.',
    affectedGuarantee: 'native wake strategy admission'
  }
  const androidStrategy = (id: BackgroundContinuationFeatureId, explanation: string) => {
    const limitation = !bindingAvailable
      ? unavailableBinding
      : !androidWake
        ? {
            code: 'companion-presence-needs-api-31-and-association',
            explanation: 'Presence-triggered continuation needs an associated, armed peer on Android API 31+.',
            affectedGuarantee: 'wake strategy admission'
          }
        : {
            code: 'application-entitlements-and-os-admission-required',
            explanation,
            affectedGuarantee: 'guaranteed wake execution or application completion'
          }
    return (bindingAvailable && androidWake ? limitedRegistration : unsupportedRegistration)(
      id,
      platform,
      implementationVersion,
      limitation
    )
  }
  if (platform === 'android') {
    return createFeatureRegistry(
      Object.freeze([
        androidWake
          ? limitedRegistration(ids.wakeOnAppearance, platform, implementationVersion, {
              code: 'companion-presence-needs-api-31-and-association',
              explanation:
                'Device presence wakes the process through Companion Device Manager (API 31+) for an armed associated peer; delivery is best-effort under Doze and vendor battery policy.',
              affectedGuarantee: 'wake of a dead process on peer appearance'
            })
          : unsupportedRegistration(ids.wakeOnAppearance, platform, implementationVersion, {
              code: 'companion-presence-needs-api-31-and-association',
              explanation:
                'Device presence observation needs Android API 31+; this device runs an older API level, so no OS wake exists.',
              affectedGuarantee: 'wake of a dead process on peer appearance'
            }),
        !bindingAvailable
          ? unsupportedRegistration(ids.nativeResubscribe, platform, implementationVersion, unavailableBinding)
          : androidWake
            ? limitedRegistration(ids.nativeResubscribe, platform, implementationVersion, {
                code: 'live-radio-qualification-pending',
                explanation:
                  'Native reconnect plus resubscribe from the wake has deterministic coverage; physical-radio qualification remains separate. Values arriving with no JS session queue in the existing bounded queues and drain with accounted loss.',
                affectedGuarantee: 'reliability-qualified wake streaming'
              })
            : unsupportedRegistration(ids.nativeResubscribe, platform, implementationVersion, {
                code: 'companion-presence-needs-api-31-and-association',
                explanation:
                  'Native resubscribe executes from the presence wake, which needs Android API 31+; this device runs an older API level.',
                affectedGuarantee: 'wake streaming without JavaScript'
              }),
        androidStrategy(
          ids.headlessTask,
          'The native presence owner dispatches the declared registered React Native task. Association, observation, permissions and OS startup admission are required; task-dispatched does not prove application work completed.'
        ),
        androidStrategy(
          ids.wakeNotification,
          'The native presence owner starts the configured connected-device foreground service. Association, observation, service/notification configuration, permissions and OS admission are required; foreground-service-started does not prove streaming or business completion.'
        )
      ])
    )
  }
  return createFeatureRegistry(
    Object.freeze([
      (facts.appleRestorationConfigured === true ? limitedRegistration : unsupportedRegistration)(
        ids.wakeOnAppearance,
        platform,
        implementationVersion,
        {
          code: 'configured-native-restoration-authority-required',
          explanation:
            'BLE relaunch requires a configured restoration authority and app background modes; restored peripherals arrive through willRestoreState. Without that configuration this host does not advertise restoration wake.',
          affectedGuarantee: 'relaunch of a terminated app on BLE events'
        }
      ),
      !bindingAvailable
        ? unsupportedRegistration(ids.nativeResubscribe, platform, implementationVersion, unavailableBinding)
        : facts.appleRestorationConfigured === true
          ? limitedRegistration(ids.nativeResubscribe, platform, implementationVersion, {
              code: 'live-radio-qualification-pending',
              explanation:
                'The configured restoration owner reconnects and resubscribes natively without JavaScript, retaining a bounded backlog with accounted loss. Physical-radio qualification remains separate.',
              affectedGuarantee: 'reliability-qualified wake streaming'
            })
          : unsupportedRegistration(ids.nativeResubscribe, platform, implementationVersion, {
              code: 'configured-native-restoration-authority-required',
              explanation: 'Native wake streaming requires a configured restoration authority in this application.',
              affectedGuarantee: 'wake streaming without JavaScript'
            }),
      unsupportedRegistration(ids.headlessTask, platform, implementationVersion, {
        code: 'android-only-wake-mechanism',
        explanation:
          'React Native Headless JS task dispatch is an Android mechanism; Apple BLE restoration uses the native restoration owner instead.',
        affectedGuarantee: 'JavaScript execution in the wake'
      }),
      unsupportedRegistration(ids.wakeNotification, platform, implementationVersion, {
        code: 'android-only-wake-mechanism',
        explanation:
          'Apple does not provide Android connected-device foreground services; BLE restoration uses the native restoration owner instead.',
        affectedGuarantee: 'live streaming under a wake notification'
      })
    ])
  )
}
