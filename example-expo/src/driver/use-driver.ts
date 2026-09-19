// example-expo/src/driver/use-driver.ts

import { useEffect, useState } from 'react'
import { remoteDriver } from './app-driver.ts'
import type { JsonObject, RemoteDriverState, Scenario, ScenarioEvent } from './shared.ts'

export interface ScenarioView {
  readonly snapshot: JsonObject
  readonly headline: string | null
  readonly events: readonly ScenarioEvent[]
}

function viewOf(scenario: Scenario): ScenarioView {
  return { snapshot: scenario.snapshot(), headline: scenario.headline(), events: scenario.recentEvents() }
}

/** Re-renders on every snapshot publish and event of one scenario. */
export function useScenarioView(scenario: Scenario): ScenarioView {
  const [view, setView] = useState(() => viewOf(scenario))
  useEffect(() => {
    setView(viewOf(scenario))
    return scenario.subscribe(() => setView(viewOf(scenario)))
  }, [scenario])
  return view
}

/** `null` in release builds, where the remote channel does not exist. */
export function useRemoteDriverState(): RemoteDriverState | null {
  const [state, setState] = useState(() => remoteDriver?.state() ?? null)
  useEffect(() => {
    if (remoteDriver === null) return undefined
    setState(remoteDriver.state())
    return remoteDriver.subscribe(setState)
  }, [])
  return state
}

/** Starts the remote channel for the app's lifetime (development builds only). */
export function useRemoteDriverLifecycle(): void {
  useEffect(() => {
    if (remoteDriver === null) return undefined
    remoteDriver.start()
    return () => remoteDriver?.stop()
  }, [])
}
