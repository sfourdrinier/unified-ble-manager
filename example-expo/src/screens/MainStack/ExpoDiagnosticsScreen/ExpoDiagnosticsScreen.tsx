import React, { useState } from 'react'
import { AppButton, AppText, AppTextInput, ScreenDefaultContainer } from '../../../components/atoms'
import { BLEService } from '../../../services'

export function ExpoDiagnosticsScreen() {
  const [output, setOutput] = useState('No diagnostic action has run.')
  const [presencePeerId, setPresencePeerId] = useState('')
  const [associationIdText, setAssociationIdText] = useState('')

  const run = async (action: () => Promise<unknown>) => {
    try {
      setOutput(JSON.stringify(await action(), null, 2))
    } catch (error) {
      setOutput(error instanceof Error ? error.message : 'The diagnostic action failed.')
    }
  }

  return (
    <ScreenDefaultContainer scrollable>
      <AppText>Expo host diagnostics and lifecycle evidence</AppText>
      <AppButton label="Readiness" onPress={() => void run(() => BLEService.readiness())} />
      <AppButton label="Redacted support bundle" onPress={() => void run(() => BLEService.redactedSupportBundle())} />
      <AppButton label="Claim native restoration (iOS)" onPress={() => void run(() => BLEService.claimRestoration())} />
      <AppButton
        label="Associate companion device"
        // Finding 222: scope the chooser to the bench strap. The name option
        // is an exact device name (Pattern.quote + CDM full-match), so this
        // passes the H10's full advertised name, matching the Polar H10
        // prefix the scan journey uses (PolarH10Session).
        onPress={() => void run(() => BLEService.associateCompanionDevice('Polar H10 E9B93D29'))}
      />
      <AppTextInput placeholder="Known peer id for presence" value={presencePeerId} onChangeText={setPresencePeerId} />
      <AppButton label="Observe presence" onPress={() => void run(() => BLEService.observePresence(presencePeerId))} />
      <AppButton label="Unobserve presence" onPress={() => void run(() => BLEService.unobservePresence(presencePeerId))} />
      <AppText>Background continuation (the wake executes the declared order)</AppText>
      <AppButton label="Continuation status" onPress={() => void run(() => BLEService.continuationStatus())} />
      <AppButton
        label="Claim continuation backlog"
        onPress={() => void run(() => BLEService.claimContinuationBacklog())}
      />
      <AppButton label="List companion associations" onPress={() => void run(() => BLEService.listCompanionAssociations())} />
      <AppTextInput
        placeholder="Association id to remove"
        value={associationIdText}
        onChangeText={setAssociationIdText}
      />
      <AppButton
        label="Disassociate companion device"
        onPress={() =>
          void run(() => {
            const associationId = Number.parseInt(associationIdText, 10)
            if (!Number.isSafeInteger(associationId) || associationId <= 0) {
              return Promise.reject(new Error('Enter a positive association id from the list above.'))
            }
            return BLEService.disassociateCompanionDevice(associationId)
          })
        }
      />
      <AppText>{output}</AppText>
    </ScreenDefaultContainer>
  )
}
