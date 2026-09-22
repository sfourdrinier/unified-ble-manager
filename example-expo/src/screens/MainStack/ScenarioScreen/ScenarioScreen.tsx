// example-expo/src/screens/MainStack/ScenarioScreen/ScenarioScreen.tsx
//
// Generic scenario UI: one button per command preset, all calling the same
// `dispatch` the remote driver calls; the snapshot and recent events render
// as they are published.

import React from 'react'
import type { NativeStackScreenProps } from '@react-navigation/native-stack'
import { ScrollView } from 'react-native'
import { AppButton, AppText, ScreenDefaultContainer } from '../../../components/atoms'
import { RemoteDriverBadge } from '../../../components/molecules'
import { scenarioRegistry } from '../../../driver/app-driver'
import { describeError, type JsonObject, type JsonValue } from '../../../driver/shared'
import { useScenarioView } from '../../../driver/use-driver'
import type { MainStackParamList } from '../../../navigation/navigators'

type ScenarioScreenProps = NativeStackScreenProps<MainStackParamList, 'SCENARIO_SCREEN'>

const VISIBLE_EVENTS = 60

export function ScenarioScreen({ route }: ScenarioScreenProps) {
  const scenario = scenarioRegistry.get(route.params.scenarioId)
  const view = useScenarioView(scenario)
  const description = scenario.describe()

  const run = (command: string, args: JsonObject) => {
    scenario.dispatch(command, args).catch(error => {
      // Already reported as a `command-failed` event (and in the snapshot); logged for the Metro console.
      console.warn(`[${scenario.id}] ${command} failed:`, JSON.stringify(describeError(error)))
    })
  }

  return (
    <ScreenDefaultContainer>
      <RemoteDriverBadge />
      <AppText style={{ fontSize: 12 }}>{scenario.description}</AppText>
      {description.commands.flatMap(command =>
        command.presets.map(preset => (
          <AppButton key={`${command.name}:${preset.label}`} label={preset.label} onPress={() => run(command.name, preset.args)} />
        ))
      )}
      <ScrollView style={{ flex: 1 }}>
        {view.headline === null ? null : <AppText style={{ fontSize: 36 }}>{view.headline}</AppText>}
        {Object.entries(view.snapshot).map(([key, value]) => (
          <AppText key={key} style={{ fontSize: 12 }}>
            {key}: {formatValue(value)}
          </AppText>
        ))}
        <AppText style={{ marginTop: 8 }}>Events (newest first):</AppText>
        {view.events
          .slice(-VISIBLE_EVENTS)
          .reverse()
          .map(event => (
            <AppText key={event.seq} style={{ fontSize: 11 }}>
              +{Math.round(event.atMs).toString()}ms {event.kind} {JSON.stringify(event.data)}
            </AppText>
          ))}
      </ScrollView>
    </ScreenDefaultContainer>
  )
}

function formatValue(value: JsonValue): string {
  if (value === null) return '-'
  if (typeof value === 'object') return JSON.stringify(value)
  return String(value)
}
