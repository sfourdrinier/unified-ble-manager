// example-expo/src/components/molecules/RemoteDriverBadge/RemoteDriverBadge.tsx

import React from 'react'
import { View } from 'react-native'
import { AppText } from '../../atoms'
import { useRemoteDriverState } from '../../../driver/use-driver'

const STATUS_COLORS: Readonly<Record<string, string>> = {
  connected: '#1b8a3a',
  connecting: '#b7791f',
  'waiting-to-reconnect': '#b7791f',
  'no-host': '#9b2c2c',
  stopped: '#718096',
  idle: '#718096'
}

export function RemoteDriverBadge() {
  const state = useRemoteDriverState()
  if (state === null) return null
  const color = STATUS_COLORS[state.status] ?? '#718096'
  const label =
    state.status === 'connected'
      ? `remote connected${state.hostId === null ? '' : ` as ${state.hostId}`} · ${state.receivedCommands.toString()} cmd`
      : state.status === 'waiting-to-reconnect'
        ? `remote offline, retry in ${Math.round((state.reconnectInMs ?? 0) / 1000).toString()} s`
        : `remote ${state.status}`
  return (
    <View style={{ backgroundColor: color, borderRadius: 6, paddingHorizontal: 8, paddingVertical: 4, marginBottom: 6 }}>
      <AppText style={{ color: 'white', fontSize: 12 }}>{label}</AppText>
      {state.lastError === null || state.status === 'connected' ? null : (
        <AppText style={{ color: 'white', fontSize: 10 }}>{state.lastError}</AppText>
      )}
      {state.droppedWhileOffline === 0 ? null : (
        <AppText style={{ color: 'white', fontSize: 10 }}>{state.droppedWhileOffline.toString()} message(s) dropped while offline</AppText>
      )}
    </View>
  )
}
