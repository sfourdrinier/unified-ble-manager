// example-expo/src/components/molecules/BleDevice/BleDevice.tsx

import React from 'react'
import type { ExamplePeer } from '../../../services/BLEService/BLEService'
import { Container } from './BleDevice.styled'
import { DeviceProperty } from './DeviceProperty/DeviceProperty'

export type BleDeviceProps = {
  readonly onPress: (peer: ExamplePeer) => void
  readonly peer: ExamplePeer
}

export function BleDevice({ peer, onPress }: BleDeviceProps) {
  const { advertisement } = peer
  return (
    <Container onPress={() => onPress(peer)}>
      <DeviceProperty name="local name" value={advertisement.localName ?? 'unavailable'} />
      <DeviceProperty name="peer ID" value={String(peer.peerId)} />
      <DeviceProperty
        name="connectable"
        value={advertisement.connectable === null ? 'unavailable' : String(advertisement.connectable)}
      />
      <DeviceProperty name="RSSI" value={advertisement.rssi === null ? 'unavailable' : advertisement.rssi.toString()} />
      <DeviceProperty name="observed at" value={peer.seenAt.toString()} />
      <DeviceProperty name="service UUIDs" value={advertisement.serviceUuids?.join(', ') ?? 'unavailable'} />
      <DeviceProperty
        name="manufacturer data"
        value={
          advertisement.manufacturerData
            ?.map(entry => `${entry.companyId.toString()}:${formatBytes(entry.data)}`)
            .join(', ') ?? 'unavailable'
        }
      />
      <DeviceProperty
        name="service data"
        value={
          advertisement.serviceData?.map(entry => `${entry.service}:${formatBytes(entry.data)}`).join(', ') ??
          'unavailable'
        }
      />
    </Container>
  )
}

function formatBytes(bytes: Readonly<Uint8Array>): string {
  return Array.from(bytes, byte => byte.toString(16).padStart(2, '0')).join(' ')
}
