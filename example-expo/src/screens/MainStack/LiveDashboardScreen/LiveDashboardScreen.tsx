// example-expo/src/screens/MainStack/LiveDashboardScreen/LiveDashboardScreen.tsx
//
// Live dashboard: one tile per Polar H10 in range, the same screen on phones
// and Apple TV. The tiles render the shared `live-dashboard` driver scenario
// snapshot (examples-shared/driver/scenarios/live-dashboard.ts), so a person
// tapping here and the control server driving the scenario run exactly the
// same code. The ECG trace is plain React Native Views; no charting dependency.

import React, { useEffect, useMemo, useState } from 'react'
import type { NativeStackScreenProps } from '@react-navigation/native-stack'
import { FlatList, Platform, Pressable, StyleSheet, View } from 'react-native'
import { AppButton, AppText, ScreenDefaultContainer } from '../../../components/atoms'
import { RemoteDriverBadge } from '../../../components/molecules'
import { scenarioRegistry } from '../../../driver/app-driver'
import { describeError, isJsonObject, type JsonObject, type JsonValue } from '../../../driver/shared'
import { useScenarioView } from '../../../driver/use-driver'
import type { MainStackParamList } from '../../../navigation/navigators'

type LiveDashboardScreenProps = NativeStackScreenProps<MainStackParamList, 'LIVE_DASHBOARD_SCREEN'>

const TILE_SCENARIO_ID = 'live-dashboard'
const GREYED_STATUSES: readonly string[] = ['lost', 'off']

interface TileView {
  readonly peerId: string
  readonly name: string | null
  readonly status: string
  readonly supervisorState: string | null
  readonly supervisorAttempt: number | null
  readonly lifecycleCause: string | null
  readonly lifecycle: readonly string[]
  readonly connectionGeneration: string | null
  readonly rssi: number | null
  readonly lastSeenAtMs: number | null
  readonly bpm: number | null
  readonly contact: string | null
  readonly rrIntervalsMs: readonly number[]
  readonly valueCount: number | null
  readonly batteryPercent: number | null
  readonly batteryDelivery: string | null
  readonly firmwareRevision: string | null
  readonly modelNumber: string | null
  readonly serialNumber: string | null
  readonly ecgDisplay: readonly number[]
  readonly ecgSamples: number | null
}

function stringOrNull(value: JsonValue | undefined): string | null {
  return typeof value === 'string' ? value : null
}

function numberOrNull(value: JsonValue | undefined): number | null {
  return typeof value === 'number' ? value : null
}

function numberArray(value: JsonValue | undefined): readonly number[] {
  if (!Array.isArray(value)) return []
  return value.filter((entry): entry is number => typeof entry === 'number')
}

function stringArray(value: JsonValue | undefined): readonly string[] {
  if (!Array.isArray(value)) return []
  return value.filter((entry): entry is string => typeof entry === 'string')
}

function recordOf(value: JsonValue | undefined): JsonObject {
  if (value === undefined || !isJsonObject(value)) return {}
  return value
}

function tileViewOf(peerId: string, value: JsonValue | undefined): TileView | null {
  const record = recordOf(value)
  if (Object.keys(record).length === 0) return null
  return {
    peerId,
    name: stringOrNull(record.name),
    status: stringOrNull(record.status) ?? 'off',
    supervisorState: stringOrNull(record.supervisorState),
    supervisorAttempt: numberOrNull(record.supervisorAttempt),
    lifecycleCause: stringOrNull(record.lifecycleCause),
    lifecycle: stringArray(record.lifecycle),
    connectionGeneration: stringOrNull(record.connectionGeneration),
    rssi: numberOrNull(record.rssi),
    lastSeenAtMs: numberOrNull(record.lastSeenAtMs),
    bpm: numberOrNull(record.bpm),
    contact: stringOrNull(record.contact),
    rrIntervalsMs: numberArray(record.rrIntervalsMs),
    valueCount: numberOrNull(record.valueCount),
    batteryPercent: numberOrNull(record.batteryPercent),
    batteryDelivery: stringOrNull(record.batteryDelivery),
    firmwareRevision: stringOrNull(record.firmwareRevision),
    modelNumber: stringOrNull(record.modelNumber),
    serialNumber: stringOrNull(record.serialNumber),
    ecgDisplay: numberArray(record.ecgDisplay),
    ecgSamples: numberOrNull(record.ecgSamples)
  }
}

function tilesOf(snapshot: JsonObject): TileView[] {
  const tiles = recordOf(snapshot.tiles)
  const tilesInOrder = stringArray(snapshot.tileOrder)
    .map(peerId => tileViewOf(peerId, tiles[peerId]))
    .filter((tile): tile is TileView => tile !== null)
  if (tilesInOrder.length > 0) return tilesInOrder
  return Object.entries(tiles)
    .map(([peerId, value]) => tileViewOf(peerId, value))
    .filter((tile): tile is TileView => tile !== null)
}

/** Seconds since the tile was last seen; `lastSeenAtMs` shares the `performance.now()` clock. */
function lastSeenAge(lastSeenAtMs: number | null): string {
  if (lastSeenAtMs === null) return 'never seen'
  const ageMs = performance.now() - lastSeenAtMs
  if (ageMs < 0) return 'just now'
  if (ageMs < 10_000) return `${Math.round(ageMs / 1000).toString()}s ago`
  return `${Math.round(ageMs / 60_000).toString()}m ago`
}

const EcgTrace = React.memo(function EcgTrace({ samples }: { samples: readonly number[] }) {
  const bars = useMemo(() => {
    if (samples.length === 0) return null
    let min = samples[0] ?? 0
    let max = min
    for (const sample of samples) {
      if (sample < min) min = sample
      if (sample > max) max = sample
    }
    const span = max - min || 1
    return samples.map((sample, index) => ({ key: index, height: 4 + Math.round((64 * (sample - min)) / span) }))
  }, [samples])
  if (bars === null) {
    return (
      <View style={styles.ecgEmpty}>
        <AppText style={styles.ecgHint}>ECG —</AppText>
      </View>
    )
  }
  return (
    <View style={styles.ecg}>
      {bars.map(bar => (
        <View key={`ecg-${bar.key.toString()}`} style={[styles.ecgBar, { height: bar.height }]} />
      ))}
    </View>
  )
})

interface DashboardTileProps {
  readonly tile: TileView
  readonly tv: boolean
  readonly selected: boolean
  readonly preferred: boolean
  readonly onPress: () => void
}

const DashboardTile = React.memo(function DashboardTile({ tile, tv, selected, preferred, onPress }: DashboardTileProps) {
  const [focused, setFocused] = useState(false)
  const greyed = GREYED_STATUSES.includes(tile.status)
  const statusLine =
    tile.supervisorState === null ? tile.status : `${tile.status} · ${tile.supervisorState}`
  const causeLine = tile.lifecycleCause === null ? null : ` · ${tile.lifecycleCause}`
  return (
    <Pressable
      onPress={onPress}
      onFocus={() => setFocused(true)}
      onBlur={() => setFocused(false)}
      hasTVPreferredFocus={preferred}
      style={[styles.tile, tv ? styles.tileTv : styles.tilePhone, greyed ? styles.tileGreyed : null, focused ? styles.tileFocused : null]}
    >
      <AppText style={tv ? styles.strapNameTv : styles.strapName}>{tile.name ?? tile.peerId}</AppText>
      <AppText style={tv ? styles.heartRateTv : styles.heartRate}>{tile.bpm === null ? '--' : tile.bpm.toString()} bpm</AppText>
      <AppText style={styles.row}>
        Contact: {tile.contact ?? '—'} · RR: {tile.rrIntervalsMs.length === 0 ? '—' : tile.rrIntervalsMs.join(', ')}
      </AppText>
      <EcgTrace samples={tile.ecgDisplay} />
      <AppText style={styles.row}>
        Battery: {tile.batteryPercent === null ? '—' : `${tile.batteryPercent.toString()}%`}
        {tile.batteryDelivery === null ? '' : ` (${tile.batteryDelivery})`} · FW: {tile.firmwareRevision ?? '—'}
      </AppText>
      <AppText style={styles.row}>
        {[tile.modelNumber, tile.serialNumber].filter(part => part !== null).join(' · ') || '—'}
        {tile.rssi === null ? '' : ` · ${tile.rssi.toString()} dBm`}
      </AppText>
      <AppText style={styles.status}>
        {statusLine}
        {causeLine}
      </AppText>
      <AppText style={styles.row}>Last seen: {lastSeenAge(tile.lastSeenAtMs)}</AppText>
      {selected ? (
        <View>
          <AppText style={styles.row}>Connection: {tile.connectionGeneration ?? '—'}</AppText>
          <AppText style={styles.row}>
            Supervisor attempt: {tile.supervisorAttempt === null ? '—' : tile.supervisorAttempt.toString()} · Values:{' '}
            {tile.valueCount === null ? '—' : tile.valueCount.toString()} · ECG samples:{' '}
            {tile.ecgSamples === null ? '—' : tile.ecgSamples.toString()}
          </AppText>
          {tile.lifecycle
            .slice(-6)
            .reverse()
            .map(line => (
              <AppText key={line} style={styles.lifecycle}>
                {line}
              </AppText>
            ))}
        </View>
      ) : null}
    </Pressable>
  )
})

export function LiveDashboardScreen(_props: LiveDashboardScreenProps) {
  const scenario = scenarioRegistry.get(TILE_SCENARIO_ID)
  const view = useScenarioView(scenario)
  const [selected, setSelected] = useState<string | null>(null)
  const tiles = useMemo(() => tilesOf(view.snapshot), [view.snapshot])
  const phase = stringOrNull(view.snapshot.phase) ?? 'idle'
  const tv = Platform.isTV

  useEffect(() => {
    if (phase !== 'idle') return
    scenario.dispatch('start', { ecg: true }).catch(error => {
      // `scenario.busy` means a second mount (or the control server) already
      // started the dashboard; observing is enough, so only real failures log.
      if (describeError(error).code !== 'scenario.busy') {
        console.warn('[live-dashboard] auto-start failed:', JSON.stringify(describeError(error)))
      }
    })
  }, [scenario, phase])

  const run = (command: string, args: JsonObject) => {
    scenario.dispatch(command, args).catch(error => {
      // Already reported as a `command-failed` event; logged for the Metro console.
      console.warn(`[live-dashboard] ${command} failed:`, JSON.stringify(describeError(error)))
    })
  }

  return (
    <ScreenDefaultContainer>
      <RemoteDriverBadge />
      <AppText style={tv ? styles.headlineTv : styles.headline}>
        {view.headline ?? phase} · {tiles.length.toString()} strap{tiles.length === 1 ? '' : 's'}
      </AppText>
      <View style={styles.controls}>
        <AppButton label="Start (ECG on)" onPress={() => run('start', { ecg: true })} />
        <AppButton label="Start without ECG" onPress={() => run('start', { ecg: false })} />
        <AppButton label="Stop" onPress={() => run('stop', {})} />
      </View>
      {tiles.length === 0 ? (
        <AppText style={tv ? styles.emptyTv : styles.empty}>No straps in range yet — put a Polar H10 on.</AppText>
      ) : (
        <FlatList
          style={styles.list}
          data={tiles}
          key={tv ? 'tv-grid' : 'phone-stack'}
          numColumns={tv ? 2 : 1}
          keyExtractor={tile => tile.peerId}
          extraData={selected}
          renderItem={({ item, index }) => (
            <DashboardTile
              tile={item}
              tv={tv}
              selected={selected === item.peerId}
              preferred={index === 0}
              onPress={() => setSelected(current => (current === item.peerId ? null : item.peerId))}
            />
          )}
        />
      )}
    </ScreenDefaultContainer>
  )
}

const styles = StyleSheet.create({
  headline: { fontSize: 22, marginBottom: 4 },
  headlineTv: { fontSize: 40, marginBottom: 8 },
  controls: { flexDirection: 'row', flexWrap: 'wrap', gap: 8, marginBottom: 8 },
  list: { flex: 1 },
  empty: { fontSize: 16, marginTop: 24 },
  emptyTv: { fontSize: 32, marginTop: 48 },
  tile: { flex: 1, margin: 8, padding: 16, borderRadius: 12, backgroundColor: '#f4f4f4', borderWidth: 3, borderColor: 'transparent' },
  tilePhone: { marginHorizontal: 0 },
  tileTv: { minHeight: 420 },
  tileGreyed: { opacity: 0.55 },
  tileFocused: { borderColor: '#d21f26' },
  strapName: { fontSize: 20 },
  strapNameTv: { fontSize: 32 },
  heartRate: { fontSize: 48, textAlign: 'center', marginVertical: 4 },
  heartRateTv: { fontSize: 72, textAlign: 'center', marginVertical: 8 },
  row: { fontSize: 12, marginTop: 2 },
  status: { fontSize: 14, marginTop: 4 },
  lifecycle: { fontSize: 11, marginTop: 1 },
  ecg: { flexDirection: 'row', alignItems: 'center', height: 76, marginVertical: 6, overflow: 'hidden' },
  ecgEmpty: { height: 76, marginVertical: 6, alignItems: 'center', justifyContent: 'center' },
  ecgHint: { fontSize: 12 },
  ecgBar: { width: 2, marginRight: 1, backgroundColor: '#d21f26', borderRadius: 1 }
})
