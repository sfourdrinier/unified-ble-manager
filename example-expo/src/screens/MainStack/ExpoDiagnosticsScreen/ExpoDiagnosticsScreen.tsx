import React, { useState } from 'react'
import { AppButton, AppText, ScreenDefaultContainer } from '../../../components/atoms'
import { BLEService } from '../../../services'

export function ExpoDiagnosticsScreen() {
  const [output, setOutput] = useState('No diagnostic action has run.')

  const run = async (action: () => Promise<unknown>) => {
    try {
      setOutput(JSON.stringify(await action(), null, 2))
    } catch (error) {
      setOutput(error instanceof Error ? error.message : 'The diagnostic action failed.')
    }
  }

  return (
    <ScreenDefaultContainer>
      <AppText>Expo host diagnostics and lifecycle evidence</AppText>
      <AppButton label="Readiness" onPress={() => void run(() => BLEService.readiness())} />
      <AppButton label="Redacted support bundle" onPress={() => void run(() => BLEService.redactedSupportBundle())} />
      <AppButton label="Claim native restoration" onPress={() => void run(() => BLEService.claimRestoration())} />
      <AppButton
        label="Associate companion device"
        onPress={() => void run(() => BLEService.associateCompanionDevice())}
      />
      <AppText>{output}</AppText>
    </ScreenDefaultContainer>
  )
}
