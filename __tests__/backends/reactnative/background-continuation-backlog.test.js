// __tests__/backends/reactnative/background-continuation-backlog.test.js
//
// BGS4: values arriving with no JS session queue in the continuation
// session's bounded outbox; the app drains them through the claim batches
// with loss accounted EXACTLY as the drain contract specifies
// (docs/MOBILE_RUST_WIRE.md): stream-end carries droppedItems/droppedBytes,
// controlLost is cumulative and drives session.reconcile. Nothing silently
// dropped: the consumer sees how much was lost and why.

const {
  aggregateContinuationClaim,
  continuationConsumerSelector
} = require('../../../src/backends/reactnative/react-native-continuation-claim')
const { normalizeBackgroundContinuation } = require('../../../src/backend-contract/background-continuation')

const HR_SERVICE = '0000180d-0000-1000-8000-00805f9b34fb'
const HR_MEASUREMENT = '00002a37-0000-1000-8000-00805f9b34fb'
// HR measurement: flags 0x00, heart rate 72 bpm.
const HR_VALUE_B64 = 'AEg='

function batch(records, { more = false, controlLost = 0 } = {}) {
  return JSON.stringify({ more, records, controlLost })
}

function valueRecord(ordinal, consumer, valueB64 = HR_VALUE_B64) {
  return { t: 'value', ordinal, consumer, valueB64, delivery: 'notification' }
}

const DECLARATION = normalizeBackgroundContinuation({
  onAppearance: 'native',
  resubscribe: [{ serviceUuid: HR_SERVICE, characteristicUuid: HR_MEASUREMENT }]
})

function claimPayload(consumerCount, batches, disposed) {
  return { consumerCount, selectors: DECLARATION.resubscribe.slice(0, consumerCount), batches, disposed, afterCutoffLoss: { items: 0, bytes: 0 } }
}

describe('continuation backlog drains with accounted loss', () => {
  it('aggregates value records across chained batches with ordinal continuity', () => {
    const claim = aggregateContinuationClaim(
      {
        ...claimPayload(1, [
          batch([valueRecord(1, 'ubm-continuation-0'), valueRecord(2, 'ubm-continuation-0')], { more: true }),
          batch([valueRecord(3, 'ubm-continuation-0')], { more: false })
        ], true)
      },
      DECLARATION
    )
    expect(claim.values).toHaveLength(3)
    expect([...claim.values[0].value]).toEqual([0, 72])
    expect(claim.values.every(record => record.consumer === 'ubm-continuation-0')).toBe(true)
    expect(claim.disposed).toBe(true)
    expect(claim.controlLost).toBe(0)
  })

  it('surfaces stream-end overflow with exact drop counts, never silent', () => {
    const claim = aggregateContinuationClaim(
      {
        ...claimPayload(1, [
          batch([
            valueRecord(1, 'ubm-continuation-0'),
            { t: 'stream-end', ordinal: 2, consumer: 'ubm-continuation-0', reason: 'overflow', droppedItems: 5, droppedBytes: 100 }
          ])
        ], true)
      },
      DECLARATION
    )
    expect(claim.values).toHaveLength(1)
    expect(claim.streamEnds).toEqual([
      { consumer: 'ubm-continuation-0', reason: 'overflow', droppedItems: 5, droppedBytes: 100 }
    ])
  })

  it('reports cumulative controlLost so the reader reconciles instead of inferring', () => {
    const claim = aggregateContinuationClaim(
      claimPayload(1, [batch([valueRecord(1, 'ubm-continuation-0')], { controlLost: 3 })], false),
      DECLARATION
    )
    expect(claim.controlLost).toBe(3)
    expect(claim.disposed).toBe(false)
  })

  it('refuses a regressed ordinal chain fail-closed instead of delivering it silently', () => {
    // The drain contract numbers every record monotonically: a batch that
    // repeats or regresses ordinals is malformed, never merged quietly.
    expect(() =>
      aggregateContinuationClaim(
        claimPayload(1, [
            batch([valueRecord(1, 'ubm-continuation-0')], { more: true }),
            batch([valueRecord(1, 'ubm-continuation-0')], { more: false })
          ], true),
        DECLARATION
      )
    ).toThrow()
  })

  it('refuses malformed batches instead of delivering partial backlogs', () => {
    expect(() => aggregateContinuationClaim(claimPayload(1, ['{nope'], true), DECLARATION)).toThrow()
  })

  it('reads empty batches as the valid no-wake answer, never an error', () => {
    const claim = aggregateContinuationClaim(claimPayload(0, [], false), DECLARATION)
    expect(claim.values).toEqual([])
    expect(claim.streamEnds).toEqual([])
    expect(claim.controlLost).toBe(0)
    expect(claim.disposed).toBe(false)
  })

  it('refuses backlog records for consumers the order never subscribed', () => {
    expect(() =>
      aggregateContinuationClaim(
        claimPayload(1, [batch([valueRecord(1, 'ubm-continuation-7')])], true),
        DECLARATION
      )
    ).toThrow()
  })

  it('maps wake consumers to the declared selectors they subscribed', () => {
    // This list came from the exact native session being claimed. It must
    // remain authoritative even when a later declaration reorders selectors.
    const pinnedSelectors = [
      {
        serviceUuid: HR_SERVICE,
        serviceOccurrence: 1,
        characteristicUuid: HR_MEASUREMENT,
        characteristicOccurrence: 1
      },
      {
        serviceUuid: HR_SERVICE,
        serviceOccurrence: 1,
        characteristicUuid: '00002a38-0000-1000-8000-00805f9b34fb',
        characteristicOccurrence: 2
      }
    ]
    expect(continuationConsumerSelector('ubm-continuation-0', pinnedSelectors)).toEqual({
      serviceUuid: HR_SERVICE,
      serviceOccurrence: 1,
      characteristicUuid: HR_MEASUREMENT,
      characteristicOccurrence: 1
    })
    expect(continuationConsumerSelector('ubm-continuation-1', pinnedSelectors)).toEqual(pinnedSelectors[1])
    expect(continuationConsumerSelector('ubm-continuation-7', pinnedSelectors)).toBeNull()
    expect(continuationConsumerSelector('s1-sub-3', pinnedSelectors)).toBeNull()
  })
})
