// example-expo/src/screens/MainStack/PolarH10Screen/PolarH10Screen.tsx

import React, { useEffect, useRef, useState } from 'react'
import { ScrollView } from 'react-native'
import { AppButton, AppText, ScreenDefaultContainer } from '../../../components/atoms'
import {
  INITIAL_POLAR_H10_SNAPSHOT,
  PolarH10Session,
  type PolarH10Snapshot
} from '../../../services/BLEService/PolarH10Session'

const RESTARTABLE_PHASES: ReadonlySet<PolarH10Snapshot['phase']> = new Set(['idle', 'stopped'])

export function PolarH10Screen() {
  const [snapshot, setSnapshot] = useState<PolarH10Snapshot>(INITIAL_POLAR_H10_SNAPSHOT)
  const sessionRef = useRef<PolarH10Session | null>(null)
  const mountedRef = useRef(true)

  useEffect(() => {
    mountedRef.current = true
    return () => {
      mountedRef.current = false
      const session = sessionRef.current
      sessionRef.current = null
      if (session !== null) {
        void session.stop().catch(error => console.error('[H10] unmount cleanup failed:', error))
      }
    }
  }, [])

  const start = () => {
    const previous = sessionRef.current
    if (previous !== null && !RESTARTABLE_PHASES.has(previous.current().phase)) {
      console.log(`[H10] start ignored in phase ${previous.current().phase}; press Stop first`)
      return
    }
    const session = new PolarH10Session(next => {
      if (mountedRef.current && sessionRef.current === session) setSnapshot(next)
    })
    sessionRef.current = session
    void session.start()
  }

  const stop = () => {
    const session = sessionRef.current
    if (session === null) return
    void session.stop().catch(error => console.error('[H10] stop failed:', error))
  }

  return (
    <ScreenDefaultContainer>
      <AppButton label="Find + connect Polar H10" onPress={start} />
      <AppButton label="Stop (remove / release / destroy)" onPress={stop} />
      <ScrollView style={{ flex: 1 }}>
        <AppText>Phase: {snapshot.phase}</AppText>
        {snapshot.error === null ? null : <AppText>Error: {snapshot.error}</AppText>}
        <AppText>Device: {snapshot.deviceName ?? '-'}</AppText>
        <AppText>Connection generation: {snapshot.connectionGeneration ?? '-'}</AppText>
        <AppText style={{ fontSize: 48 }}>{snapshot.bpm === null ? '--' : snapshot.bpm.toString()} bpm</AppText>
        <AppText>Contact: {snapshot.contact ?? '-'}</AppText>
        <AppText>RR (ms): {snapshot.rrIntervalsMs.length === 0 ? '-' : snapshot.rrIntervalsMs.join(', ')}</AppText>
        <AppText>Values: {snapshot.valueCount.toString()}</AppText>
        <AppText>Max gap: {snapshot.maxGapMs === null ? '-' : `${Math.round(snapshot.maxGapMs).toString()} ms`}</AppText>
        <AppText>
          Delivery: requested {snapshot.requestedDelivery ?? '-'} / effective {snapshot.effectiveDelivery ?? '-'} / last
          value {snapshot.lastValueDelivery ?? '-'}
        </AppText>
        <AppText>Lifecycle events:</AppText>
        {snapshot.lifecycleEvents.map((line, index) => (
          <AppText key={`lifecycle-${index.toString()}`}>  {line}</AppText>
        ))}
        <AppText>Stream notices:</AppText>
        {snapshot.streamNotices.map((line, index) => (
          <AppText key={`notice-${index.toString()}`}>  {line}</AppText>
        ))}
        <AppText>Cleanup:</AppText>
        {snapshot.cleanup.map(step => (
          <AppText key={step.step}>
            {'  '}
            {step.step}: {step.state} ({step.detail})
          </AppText>
        ))}
      </ScrollView>
    </ScreenDefaultContainer>
  )
}
