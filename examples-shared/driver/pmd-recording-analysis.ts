import { parseAccFrame, parseEcgFrame } from './polar-pmd.ts'
import { isPmdRecording } from './pmd-recording.ts'
import { isJsonObject } from './protocol.ts'
import type { JsonObject } from './protocol.ts'

type Axis = { min: number; max: number; total: number }
type Stream = {
  peerId: string
  generation: string | null
  measurement: number
  settings: JsonObject
  packets: number
  samples: number
  firstPacketSamples: number
  firstTimestamp: bigint
  lastTimestamp: bigint
  timestampDiscontinuities: number
  axes: Record<string, Axis>
}

/** Structural/signal statistics, not a claim that two different physical motions match.
 * Re-decodes retained bytes with the same parser used live; never trusts saved sample counts. */
export function summarizePmdRecording(value: unknown): JsonObject {
  if (!isPmdRecording(value) || !Array.isArray(value.records)) throw new Error('invalid PMD recording')
  const streams = new Map<string, Stream>()
  const errors: JsonObject[] = []
  const lossRecords: JsonObject[] = []
  for (const record of value.records) {
    if (!isJsonObject(record) || !isJsonObject(record.data)) continue // Already validated at boundary.
    if (record.kind === 'loss' || record.kind === 'error') lossRecords.push(record)
    if (record.kind !== 'packet') continue
    try {
      const hex = record.data.bytesHex
      if (typeof hex !== 'string' || !/^(?:[0-9a-fA-F]{2})+$/.test(hex)) throw new Error('invalid raw packet hex')
      const bytes = Uint8Array.from(hex.match(/../g) ?? [], byte => Number.parseInt(byte, 16))
      const measurement = bytes[0]
      if (measurement !== 0 && measurement !== 2) throw new Error(`unsupported PMD measurement ${String(measurement)}`)
      const frame = measurement === 2 ? parseAccFrame(bytes) : parseEcgFrame(bytes)
      const samples =
        'samplesMilliG' in frame
          ? frame.samplesMilliG.map(sample => ({ x: sample.x, y: sample.y, z: sample.z }))
          : frame.samplesMicroVolts.map(ecg => ({ ecg }))
      if (typeof record.peerId !== 'string' || (record.generation !== null && typeof record.generation !== 'string'))
        throw new Error('invalid stream identity')
      const settings = isJsonObject(record.data.settings) ? record.data.settings : {}
      // Include the PMD session and negotiated settings: a restart is not a continuous sample clock.
      const key = JSON.stringify([
        record.peerId,
        record.generation,
        record.data.pmdGeneration ?? null,
        measurement,
        settings
      ])
      let stream = streams.get(key)
      if (stream === undefined) {
        stream = {
          peerId: record.peerId,
          generation: record.generation,
          measurement,
          settings,
          packets: 0,
          samples: 0,
          firstPacketSamples: samples.length,
          firstTimestamp: frame.timestampNs,
          lastTimestamp: frame.timestampNs,
          timestampDiscontinuities: 0,
          axes: {}
        }
        streams.set(key, stream)
      } else {
        const deltaNs = Number(frame.timestampNs - stream.lastTimestamp)
        const rate = settings.sampleRateHz
        const expected = typeof rate === 'number' && rate > 0 ? (samples.length * 1e9) / rate : null
        // Allow sub-sample clock jitter, but a whole missing sample must be visible.
        const tolerance = typeof rate === 'number' && rate > 0 ? 0.5e9 / rate : 0
        if (deltaNs <= 0 || (expected !== null && Math.abs(deltaNs - expected) > tolerance))
          stream.timestampDiscontinuities += 1
      }
      stream.lastTimestamp = frame.timestampNs
      stream.packets += 1
      stream.samples += samples.length
      for (const sample of samples)
        for (const [axis, sampleValue] of Object.entries(sample)) {
          const stats = stream.axes[axis]
          if (stats === undefined) stream.axes[axis] = { min: sampleValue, max: sampleValue, total: sampleValue }
          else {
            stats.min = Math.min(stats.min, sampleValue)
            stats.max = Math.max(stats.max, sampleValue)
            stats.total += sampleValue
          }
        }
    } catch (error) {
      errors.push({
        peerId: record.peerId ?? null,
        generation: record.generation ?? null,
        atMs: record.atMs ?? null,
        message: error instanceof Error ? error.message : String(error)
      })
    }
  }
  return {
    schema: 'ubm-pmd-comparison-summary/1',
    metadata: value.metadata ?? {},
    summary: value.summary ?? {},
    equivalenceEstablished: false,
    errors,
    lossRecords,
    streams: [...streams.values()].map(stream => ({
      peerId: stream.peerId,
      generation: stream.generation,
      measurement: stream.measurement,
      units: stream.measurement === 2 ? 'milli-g' : 'microvolt',
      settings: stream.settings,
      packets: stream.packets,
      samples: stream.samples,
      firstTimestampNs: stream.firstTimestamp.toString(),
      lastTimestampNs: stream.lastTimestamp.toString(),
      measuredRateHz:
        stream.lastTimestamp > stream.firstTimestamp
          ? ((stream.samples - stream.firstPacketSamples) * 1e9) / Number(stream.lastTimestamp - stream.firstTimestamp)
          : null,
      timestampDiscontinuities: stream.timestampDiscontinuities,
      axes: Object.fromEntries(
        Object.entries(stream.axes).map(([axis, stats]) => [
          axis,
          { min: stats.min, max: stats.max, mean: stats.total / stream.samples }
        ])
      )
    }))
  }
}
