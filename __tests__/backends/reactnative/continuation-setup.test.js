const {
  normalizeContinuationSetup,
  serializeContinuationSetup,
  deserializeContinuationSetup
} = require('../../../src/backend-contract/continuation-setup')
const {
  normalizeBackgroundContinuation,
  serializeBackgroundContinuation
} = require('../../../src/backend-contract/background-continuation')
const { validateUnifiedBleExpoPluginOptions } = require('../../../plugin/src/expoPluginSchema')

const selector = {
  serviceUuid: '0000180d-0000-1000-8000-00805f9b34fb',
  serviceOccurrence: 1,
  characteristicUuid: '00002a39-0000-1000-8000-00805f9b34fb',
  characteristicOccurrence: 1
}
const step = () => ({
  selector,
  value: new Uint8Array([2, 0]),
  timeoutMs: 3000,
  response: {
    subscriptionIndex: 0,
    prefix: new Uint8Array([240, 2, 0]),
    minLength: 5,
    maxLength: 5,
    status: { offset: 3, accepted: [0] }
  }
})

describe('bounded native continuation setup contract', () => {
  it.each([3, 512])('requires space after a %i-byte prefix for the mandatory status byte', length => {
    const input = step()
    input.response.prefix = new Uint8Array(length)
    input.response.minLength = length
    input.response.maxLength = length
    input.response.status.offset = length
    expect(() => normalizeContinuationSetup([input], 1)).toThrow(
      expect.objectContaining({
        normalized: expect.objectContaining({
          code: 'argument.invalid',
          operation: 'background.continuation.setup.response.minLength'
        })
      })
    )
  })
  it('requires explicit bounded recording identity and quotas on the native strategy', () => {
    const recording = { id: 'h10_session-1', maxBytes: 1048576, maxRecords: 10000 }
    const input = { onAppearance: 'native', resubscribe: [], recording }
    expect(normalizeBackgroundContinuation(input).recording).toEqual(recording)
    expect(JSON.parse(serializeBackgroundContinuation(normalizeBackgroundContinuation(input))).recording).toEqual(
      recording
    )
    expect(
      validateUnifiedBleExpoPluginOptions({ background: { continuation: input } }).background.continuation.recording
    ).toEqual(recording)
    for (const invalidRecording of [
      null,
      {},
      { ...recording, id: '../escape' },
      { ...recording, id: '' },
      { ...recording, id: 'x'.repeat(65) },
      { ...recording, maxBytes: 1048575 },
      { ...recording, maxBytes: 1073741825 },
      { ...recording, maxRecords: 0 },
      { ...recording, maxRecords: 1000001 },
      { ...recording, maxRecords: 1.5 },
      { ...recording, path: '/tmp/recording' }
    ]) {
      const invalid = { ...input, recording: invalidRecording }
      expect(() => normalizeBackgroundContinuation(invalid)).toThrow()
      expect(() => validateUnifiedBleExpoPluginOptions({ background: { continuation: invalid } })).toThrow()
    }
    for (const onAppearance of ['record-only', 'headless-task', 'foreground-service']) {
      expect(() => normalizeBackgroundContinuation({ ...input, onAppearance })).toThrow()
    }
  })
  it('declares MTU negotiation with an explicit unsupported policy, not an inferred minimum', () => {
    const link = { mtu: { requested: 512, timeoutMs: 10000, onUnsupported: 'continue' } }
    const input = { onAppearance: 'native', resubscribe: [], link }
    expect(normalizeBackgroundContinuation(input).link).toEqual(link)
    expect(JSON.parse(serializeBackgroundContinuation(normalizeBackgroundContinuation(input))).link).toEqual(link)
    expect(
      validateUnifiedBleExpoPluginOptions({ background: { continuation: input } }).background.continuation.link
    ).toEqual(link)
    for (const mtu of [
      { ...link.mtu, requested: 22 },
      { ...link.mtu, requested: 518 },
      { ...link.mtu, timeoutMs: 0 },
      { ...link.mtu, timeoutMs: 20001 },
      { ...link.mtu, onUnsupported: undefined },
      { ...link.mtu, onUnsupported: 'ignore' }
    ]) {
      const invalid = { ...input, link: { mtu } }
      expect(() => normalizeBackgroundContinuation(invalid)).toThrow()
      expect(() => validateUnifiedBleExpoPluginOptions({ background: { continuation: invalid } })).toThrow()
    }
    expect(() => normalizeBackgroundContinuation({ ...input, onAppearance: 'record-only' })).toThrow()
  })
  it('preserves a bounded optional final-byte constraint', () => {
    const input = step()
    input.response.minLength = 4
    input.response.trailing = { offset: 4, accepted: [0] }
    expect(normalizeContinuationSetup([input], 1)[0].response.trailing).toEqual({ offset: 4, accepted: [0] })
    for (const trailing of [
      { offset: 3, accepted: [0] },
      { offset: 4, accepted: [] },
      { offset: 4, accepted: [0, 0] },
      { offset: 4, accepted: [256] }
    ]) {
      expect(() => normalizeContinuationSetup([{ ...input, response: { ...input.response, trailing } }], 1)).toThrow()
    }
  })
  it.each([-1, 256, 1.5, null, '2'])('refuses JSON bytes before coercion (%p)', byte => {
    const wire = serializeContinuationSetup(normalizeContinuationSetup([step()], 1))
    wire[0].value = [byte]
    expect(() => deserializeContinuationSetup(wire, 1)).toThrow()
    expect(() =>
      validateUnifiedBleExpoPluginOptions({
        background: {
          continuation: {
            onAppearance: 'native',
            resubscribe: [selector],
            setup: wire
          }
        }
      })
    ).toThrow()
  })

  it('uses generated copies of the canonical validator for the separately built Expo plugin', () => {
    const fs = require('node:fs')
    const path = require('node:path')
    const root = path.resolve(__dirname, '../../..')
    for (const file of ['continuation-selector.ts', 'continuation-setup.ts']) {
      const source = fs.readFileSync(path.join(root, 'src/backend-contract', file), 'utf8')
      const generated = fs.readFileSync(path.join(root, 'plugin/src/generated', file), 'utf8')
      expect(generated).toContain(`Generated from src/backend-contract/${file}`)
      expect(generated).toContain(source.substring(source.indexOf('/**')).trim())
    }
  })
  it('preserves the JSON recipe through Expo build-time validation', () => {
    const setup = serializeContinuationSetup(normalizeContinuationSetup([step()], 1))
    const declaration = { onAppearance: 'native', resubscribe: [selector], setup }
    const validated = validateUnifiedBleExpoPluginOptions({ background: { continuation: declaration } })
    expect(validated.background.continuation.setup).toEqual(setup)
    expect(() =>
      validateUnifiedBleExpoPluginOptions({
        background: {
          continuation: { ...declaration, onAppearance: 'record-only' }
        }
      })
    ).toThrow()
  })
  it('admits setup only on an explicit native standing order and preserves its wire bytes', () => {
    const declaration = normalizeBackgroundContinuation({
      onAppearance: 'native',
      resubscribe: [selector],
      setup: [step()]
    })
    expect(declaration.setup).toHaveLength(1)
    expect(JSON.parse(serializeBackgroundContinuation(declaration)).setup[0].value).toEqual([2, 0])
    expect(() => normalizeBackgroundContinuation({ resubscribe: [selector], setup: [step()] })).toThrow()
    expect(() => normalizeBackgroundContinuation({ onAppearance: 'native', setup: [step()] })).toThrow()
  })
  it('copies caller bytes and serializes JSON bytes without Base64 public values', () => {
    const source = step()
    const result = normalizeContinuationSetup([source], 1)
    source.value[0] = 99
    source.response.prefix[0] = 99
    source.response.status.accepted.push(6)
    expect(result[0].value).toEqual(new Uint8Array([2, 0]))
    expect(result[0].response.prefix).toEqual(new Uint8Array([240, 2, 0]))
    expect(result[0].response.status.accepted).toEqual([0])
    expect(JSON.parse(JSON.stringify(serializeContinuationSetup(result)))[0]).toEqual({
      ...step(),
      value: [2, 0],
      response: { ...step().response, prefix: [240, 2, 0] }
    })
  })

  it('permits ATT-only writes but requires an explicit deadline', () => {
    expect(normalizeContinuationSetup([{ selector, value: new Uint8Array([1]), timeoutMs: 100 }], 0)).toHaveLength(1)
    expect(() => normalizeContinuationSetup([{ selector, value: new Uint8Array([1]) }], 0)).toThrow()
  })

  it.each([
    input => {
      input.response.subscriptionIndex = 1
    },
    input => {
      input.response.subscriptionIndex = -1
    },
    input => {
      input.response.prefix = new Uint8Array()
    },
    input => {
      input.response.minLength = 2
    },
    input => {
      input.response.maxLength = 4
    },
    input => {
      input.response.status.offset = 5
    },
    input => {
      input.response.status.accepted = []
    },
    input => {
      input.response.status.accepted = [256]
    },
    input => {
      input.response.status.accepted = [0, 0]
    },
    input => {
      input.value = [2, 0]
    },
    input => {
      input.value = new Uint8Array(513)
    },
    input => {
      input.timeoutMs = Infinity
    },
    input => {
      input.timeoutMs = 0
    },
    input => {
      input.timeoutMs = 20001
    },
    input => {
      input.retry = true
    },
    input => {
      input.response.unknown = true
    },
    input => {
      input.selector.extra = true
    }
  ])('rejects malformed, unbounded, or undeclared work (%#)', mutate => {
    const input = step()
    input.selector = { ...selector }
    mutate(input)
    expect(() => normalizeContinuationSetup([input], 1)).toThrow()
  })

  it('bounds total steps and the combined setup deadline', () => {
    expect(() => normalizeContinuationSetup(Array.from({ length: 17 }, step), 1)).toThrow()
    expect(() =>
      normalizeContinuationSetup(
        Array.from({ length: 4 }, () => ({ ...step(), timeoutMs: 20000 })),
        1
      )
    ).toThrow()
    expect(normalizeContinuationSetup(undefined, 0)).toEqual([])
  })
})
