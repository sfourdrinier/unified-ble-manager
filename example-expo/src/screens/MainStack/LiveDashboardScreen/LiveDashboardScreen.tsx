// example-expo/src/screens/MainStack/LiveDashboardScreen/LiveDashboardScreen.tsx
//
// Live dashboard: one tile per Polar H10 in range, the same screen on phones
// and Apple TV. The tiles render the shared `live-dashboard` driver scenario
// snapshot (examples-shared/driver/scenarios/live-dashboard.ts), so a person
// tapping here and the control server driving the scenario run exactly the
// same code. The ECG trace is plain React Native Views; no charting dependency.

import React, { useEffect, useMemo, useState } from 'react'
import type { NativeStackScreenProps } from '@react-navigation/native-stack'
import { Alert, FlatList, Platform, Pressable, StyleSheet, TextInput, View } from 'react-native'
import { AppButton, AppText, ScreenDefaultContainer } from '../../../components/atoms'
import { RemoteDriverBadge } from '../../../components/molecules'
import { scenarioRegistry } from '../../../driver/app-driver'
import { describeError, isJsonObject, type JsonObject, type JsonValue } from '../../../driver/shared'
import { useScenarioView } from '../../../driver/use-driver'
import type { MainStackParamList } from '../../../navigation/navigators'
import { accTileView } from '../../../driver/live-dashboard-acc-view'
import { exportNativeRecording } from '../../../driver/native-recording-export'
import { recordingControls } from '../../../driver/recording-controls'
import { streamToggleProps } from '../../../driver/stream-toggle-props'
import {
  H10_ACC_SAMPLE_RATES_HZ,
  H10_ACC_RANGES_G,
  type H10AccSampleRateHz,
  type H10AccRangeG
} from '../../../driver/shared'
import { isPmdRecording } from '../../../driver/shared'

type LiveDashboardScreenProps = NativeStackScreenProps<MainStackParamList, 'LIVE_DASHBOARD_SCREEN'>

const TILE_SCENARIO_ID = 'live-dashboard'
const GREYED_STATUSES: readonly string[] = ['lost', 'off']

function StreamToggle({
  label,
  checked,
  onChange
}: {
  label: string
  checked: boolean
  onChange: (value: boolean) => void
}) {
  const [focused, setFocused] = useState(false)
  return (
    <Pressable
      {...streamToggleProps(label, checked, onChange)}
      onFocus={() => setFocused(true)}
      onBlur={() => setFocused(false)}
      style={[
        styles.option,
        checked ? styles.optionSelected : null,
        focused ? { borderColor: '#175cd3', boxShadow: '0 0 0 2px #175cd3' } : null
      ]}
    >
      <AppText>
        {label}: {checked ? 'On' : 'Off'}
      </AppText>
    </Pressable>
  )
}

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
  readonly acc: ReturnType<typeof accTileView>
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
    ecgSamples: numberOrNull(record.ecgSamples),
    acc: accTileView(record)
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

const ACC_AXES = [
  { key: 'x', color: '#b42318' },
  { key: 'y', color: '#087443' },
  { key: 'z', color: '#175cd3' }
] as const

const AccTrace = React.memo(function AccTrace({ acc }: { acc: TileView['acc'] }) {
  const extent = (acc.rangeG ?? 8) * 1000
  return (
    <View style={styles.accSection}>
      <AppText selectable style={styles.row}>
        ACC · {acc.sampleRateHz ?? '—'} Hz · ±{acc.rangeG ?? '—'} g · {acc.samples ?? 0} samples
      </AppText>
      <View
        style={styles.accTrace}
        accessible
        accessibilityLabel={`Accelerometer trace in milli-g; full scale plus or minus ${extent}`}
      >
        <View style={styles.accZero} />
        {acc.points.length === 0 ? (
          <AppText style={styles.ecgHint}>No ACC samples</AppText>
        ) : (
          ACC_AXES.map(axis =>
            acc.points.map((point, index) => (
              <View
                key={`${axis.key}-${index}`}
                style={{
                  position: 'absolute',
                  width: 3,
                  height: 3,
                  borderRadius: 2,
                  backgroundColor: axis.color,
                  left: `${(100 * index) / Math.max(1, acc.points.length - 1)}%`,
                  top: 44 - 42 * Math.max(-1, Math.min(1, point[axis.key] / extent))
                }}
              />
            ))
          )
        )}
      </View>
      <View style={styles.controls}>
        {ACC_AXES.map(axis => (
          <AppText selectable key={axis.key} style={[styles.row, { color: axis.color }]}>
            {axis.key.toUpperCase()}: {acc.last?.[axis.key] ?? '—'} mg
          </AppText>
        ))}
      </View>
      <AppText selectable style={styles.row}>
        PMD loss: {acc.loss}
      </AppText>
      {acc.error === null ? null : (
        <AppText selectable style={styles.error}>
          {acc.error}
        </AppText>
      )}
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

const DashboardTile = React.memo(function DashboardTile({
  tile,
  tv,
  selected,
  preferred,
  onPress
}: DashboardTileProps) {
  const [focused, setFocused] = useState(false)
  const greyed = GREYED_STATUSES.includes(tile.status)
  const statusLine = tile.supervisorState === null ? tile.status : `${tile.status} · ${tile.supervisorState}`
  const causeLine = tile.lifecycleCause === null ? null : ` · ${tile.lifecycleCause}`
  return (
    <Pressable
      onPress={onPress}
      onFocus={() => setFocused(true)}
      onBlur={() => setFocused(false)}
      hasTVPreferredFocus={preferred}
      style={[
        styles.tile,
        tv ? styles.tileTv : styles.tilePhone,
        greyed ? styles.tileGreyed : null,
        focused ? styles.tileFocused : null
      ]}
    >
      <AppText style={tv ? styles.strapNameTv : styles.strapName}>{tile.name ?? tile.peerId}</AppText>
      <AppText style={tv ? styles.heartRateTv : styles.heartRate}>
        {tile.bpm === null ? '--' : tile.bpm.toString()} bpm
      </AppText>
      <AppText style={styles.row}>
        Contact: {tile.contact ?? '—'} · RR: {tile.rrIntervalsMs.length === 0 ? '—' : tile.rrIntervalsMs.join(', ')}
      </AppText>
      <AppText selectable style={styles.row}>
        ECG · µV · {tile.ecgSamples ?? 0} samples
      </AppText>
      <EcgTrace samples={tile.ecgDisplay} />
      <AccTrace acc={tile.acc} />
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
  const [ecg, setEcg] = useState(true)
  const [acc, setAcc] = useState(false)
  const [accSampleRateHz, setAccSampleRateHz] = useState<H10AccSampleRateHz>(200)
  const [accRangeG, setAccRangeG] = useState<H10AccRangeG>(8)
  const [commandError, setCommandError] = useState<string | null>(null)
  const [exportStatus, setExportStatus] = useState<string | null>(null)
  const [exporting, setExporting] = useState(false)
  const [recordingLabel, setRecordingLabel] = useState('')
  const [recordingNotes, setRecordingNotes] = useState('')
  const tiles = useMemo(() => tilesOf(view.snapshot), [view.snapshot])
  const phase = stringOrNull(view.snapshot.phase) ?? 'idle'
  const tv = Platform.isTV
  const canStart = ['idle', 'stopped', 'failed'].includes(phase)
  const recording = recordOf(view.snapshot.recording)
  const recordingPhase = stringOrNull(recording.phase) ?? 'empty'
  const recordingButtons = recordingControls(recordingPhase, recording.accepting === true, exporting)

  useEffect(() => {
    if (phase !== 'idle') return
    scenario.dispatch('start', { ecg: true }).catch(error => {
      // `scenario.busy` means a second mount (or the control server) already
      // started the dashboard; observing is enough, so only real failures log.
      if (describeError(error).code !== 'scenario.busy') {
        setCommandError(JSON.stringify(describeError(error)))
        console.warn('[live-dashboard] auto-start failed:', JSON.stringify(describeError(error)))
      }
    })
  }, [scenario, phase])

  const run = (command: string, args: JsonObject) => {
    setCommandError(null)
    scenario.dispatch(command, args).catch(error => {
      setCommandError(JSON.stringify(describeError(error)))
      // Already reported as a `command-failed` event; logged for the Metro console.
      console.warn(`[live-dashboard] ${command} failed:`, JSON.stringify(describeError(error)))
    })
  }

  const exportRecording = async () => {
    setExporting(true)
    setExportStatus(null)
    try {
      const recording = await scenario.dispatch('record-export', {})
      if (!isPmdRecording(recording)) throw new Error('The recording export failed its versioned schema validation')
      const result = await exportNativeRecording(recording)
      const sharing =
        result.sharing === 'failed'
          ? `Sharing failed: ${result.error}`
          : result.sharing === 'closed'
            ? 'Share sheet closed. External save or cancellation cannot be confirmed.'
            : 'Sharing is unavailable on this host.'
      setExportStatus(`Verified local file: ${result.uri}\n${sharing}`)
    } catch (error) {
      setExportStatus(`Export failed: ${error instanceof Error ? error.message : JSON.stringify(describeError(error))}`)
    } finally {
      setExporting(false)
    }
  }

  return (
    <ScreenDefaultContainer>
      <FlatList
        contentInsetAdjustmentBehavior="automatic"
        style={styles.list}
        data={tiles}
        key={tv ? 'tv-grid' : 'phone-stack'}
        numColumns={tv ? 2 : 1}
        keyExtractor={tile => tile.peerId}
        extraData={selected}
        ListHeaderComponent={
          <View>
            <RemoteDriverBadge />
            <AppText style={tv ? styles.headlineTv : styles.headline}>
              {view.headline ?? phase} · {tiles.length.toString()} strap{tiles.length === 1 ? '' : 's'}
            </AppText>
            <View style={styles.controls}>
              <StreamToggle label="ECG" checked={ecg} onChange={setEcg} />
              <StreamToggle label="ACC" checked={acc} onChange={setAcc} />
            </View>
            <AppText selectable style={styles.row}>
              Next start settings. Stop acquisition before applying changes.
            </AppText>
            <View style={styles.controls}>
              {H10_ACC_SAMPLE_RATES_HZ.map(rate => (
                <Pressable
                  accessibilityRole="button"
                  accessibilityState={{ selected: rate === accSampleRateHz }}
                  key={rate}
                  onPress={() => setAccSampleRateHz(rate)}
                  style={[styles.option, rate === accSampleRateHz ? styles.optionSelected : null]}
                >
                  <AppText>{rate} Hz</AppText>
                </Pressable>
              ))}
            </View>
            <View style={styles.controls}>
              {H10_ACC_RANGES_G.map(range => (
                <Pressable
                  accessibilityRole="button"
                  accessibilityState={{ selected: range === accRangeG }}
                  key={range}
                  onPress={() => setAccRangeG(range)}
                  style={[styles.option, range === accRangeG ? styles.optionSelected : null]}
                >
                  <AppText>±{range} g</AppText>
                </Pressable>
              ))}
            </View>
            <View style={styles.controls}>
              <AppButton
                label="Start selected streams"
                disabled={!canStart}
                onPress={() => run('start', { ecg, acc, accSampleRateHz, accRangeG })}
              />
              <AppButton label="Stop" onPress={() => run('stop', {})} />
            </View>
            <AppText selectable style={styles.row}>
              Recording stays in this app until you explicitly export/share it. Includes device identifiers and sensor
              data.
            </AppText>
            <AppText selectable style={styles.row}>
              Recording: {recordingPhase} · {numberOrNull(recording.records) ?? 0} records ·{' '}
              {numberOrNull(recording.bytes) ?? 0} bytes
            </AppText>
            <AppText selectable style={styles.row}>
              Omitted: {numberOrNull(recording.omittedRecords) ?? 0} ·{' '}
              {stringArray(recording.incompleteReasons).join('; ') || 'No incompleteness reported'}
            </AppText>
            {recordingPhase === 'capacity-reached' ? (
              <AppText selectable style={styles.error}>
                Recording capacity reached. This capture is incomplete; export preserves that fact.
              </AppText>
            ) : null}
            <AppText selectable style={styles.row}>
              For full setup evidence: Stop acquisition, Record, then Start selected streams. Stop recording before
              export. Records remain in memory until Clear or app termination; export keeps the retained capture.
            </AppText>
            <TextInput
              accessibilityLabel="Recording label"
              placeholder="Label, e.g. real H10 or simulator"
              value={recordingLabel}
              maxLength={120}
              onChangeText={setRecordingLabel}
              editable={recordingPhase === 'empty'}
              style={styles.input}
            />
            <TextInput
              accessibilityLabel="Recording notes"
              placeholder="Optional comparison notes"
              value={recordingNotes}
              maxLength={2000}
              onChangeText={setRecordingNotes}
              editable={recordingPhase === 'empty'}
              style={styles.input}
            />
            <View style={styles.controls}>
              <AppButton
                label="Record"
                disabled={!recordingButtons.record}
                onPress={() => run('record-start', { label: recordingLabel, notes: recordingNotes })}
              />
              <AppButton
                label="Stop recording"
                disabled={!recordingButtons.stop}
                onPress={() => run('record-stop', {})}
              />
              <AppButton
                label={exporting ? 'Exporting…' : 'Export JSON file'}
                disabled={!recordingButtons.export}
                onPress={() => {
                  void exportRecording()
                }}
              />
              <AppButton
                label="Clear recording"
                disabled={!recordingButtons.clear}
                onPress={() =>
                  Alert.alert(
                    'Discard retained recording?',
                    'Export first to keep a local JSON copy. Clearing cannot be undone.',
                    [
                      { text: 'Cancel', style: 'cancel' },
                      { text: 'Discard', style: 'destructive', onPress: () => run('record-clear', {}) }
                    ]
                  )
                }
              />
            </View>
            {commandError === null ? null : (
              <AppText selectable style={styles.error}>
                {commandError}
              </AppText>
            )}
            {exportStatus === null ? null : (
              <AppText selectable style={styles.row}>
                {exportStatus}
              </AppText>
            )}
          </View>
        }
        ListEmptyComponent={
          <AppText style={tv ? styles.emptyTv : styles.empty}>No straps in range yet — put a Polar H10 on.</AppText>
        }
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
  tile: {
    flex: 1,
    margin: 8,
    padding: 16,
    borderRadius: 12,
    backgroundColor: '#f4f4f4',
    borderWidth: 3,
    borderColor: 'transparent'
  },
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
  ecgBar: { width: 2, marginRight: 1, backgroundColor: '#d21f26', borderRadius: 1 },
  accSection: { gap: 3 },
  accTrace: {
    height: 90,
    marginVertical: 4,
    overflow: 'hidden',
    justifyContent: 'center',
    alignItems: 'center',
    backgroundColor: '#ffffff',
    borderRadius: 6
  },
  accZero: { position: 'absolute', top: 45, width: '100%', height: 1, backgroundColor: '#d0d5dd' },
  option: { padding: 10, borderWidth: 1, borderRadius: 8, borderColor: '#667085' },
  optionSelected: { borderColor: '#d21f26', backgroundColor: '#fee4e2' },
  error: { color: '#b42318', fontSize: 13, marginVertical: 4 },
  input: { minHeight: 44, borderWidth: 1, borderColor: '#667085', borderRadius: 8, padding: 10, marginVertical: 4 }
})
