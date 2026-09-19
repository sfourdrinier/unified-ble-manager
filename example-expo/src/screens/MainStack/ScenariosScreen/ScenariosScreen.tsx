// example-expo/src/screens/MainStack/ScenariosScreen/ScenariosScreen.tsx

import React from 'react'
import type { NativeStackScreenProps } from '@react-navigation/native-stack'
import { ScrollView } from 'react-native'
import { AppButton, AppText, ScreenDefaultContainer } from '../../../components/atoms'
import { RemoteDriverBadge } from '../../../components/molecules'
import { scenarioRegistry } from '../../../driver/app-driver'
import type { MainStackParamList } from '../../../navigation/navigators'

type ScenariosScreenProps = NativeStackScreenProps<MainStackParamList, 'SCENARIOS_SCREEN'>

export function ScenariosScreen({ navigation }: ScenariosScreenProps) {
  return (
    <ScreenDefaultContainer>
      <RemoteDriverBadge />
      <ScrollView style={{ flex: 1 }}>
        {scenarioRegistry.list().map(scenario => (
          <React.Fragment key={scenario.id}>
            <AppButton
              label={scenario.title}
              onPress={() =>
                scenario.id === 'live-dashboard'
                  ? navigation.navigate('LIVE_DASHBOARD_SCREEN')
                  : navigation.navigate('SCENARIO_SCREEN', { scenarioId: scenario.id })
              }
            />
            <AppText style={{ fontSize: 12, marginBottom: 8 }}>{scenario.description}</AppText>
          </React.Fragment>
        ))}
      </ScrollView>
    </ScreenDefaultContainer>
  )
}
