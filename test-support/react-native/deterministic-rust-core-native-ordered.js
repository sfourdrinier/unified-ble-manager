// Argument readers for the current ubm-mobile session ordered operations.
// This is a parser for a radio-less test double, not an ordered source.
const { Buffer: NodeBuffer } = require('node:buffer')
const group = ['peerId', 'collectionId']
const reclaim = [...group, 'recordingId', 'reclaimCapability']
const ORDERED_SCHEMAS = {
  'gatt.ordered.version': [[], []],
  'gatt.ordered.failure-metadata.version': [[], []],
  'gatt.ordered.capacity.version': [[], []],
  'gatt.ordered.increase-capacity': [[...group, 'sourceGeneration', 'maxBytes', 'wireVersion'], []],
  'gatt.ordered.reserve': [['peerId'], []],
  'gatt.ordered.reclaim': [[...group, 'reclaimCapability'], ['recordingId']],
  'gatt.ordered.open': [[...group, 'wireVersion', 'connectionGeneration', 'databaseGeneration',
    'publicConnectionGeneration', 'publicDatabaseGeneration', 'members', 'maxItems', 'maxBytes', 'maxControls', 'operationId'], ['budgetMs', 'failureMetadataVersion']],
  'gatt.ordered.recording-reclaim.version': [[], []],
  'gatt.ordered.recording-reclaim.next': [reclaim, ['callerKey']],
  'gatt.ordered.marker.version': [[], []],
  'gatt.ordered.marker.reserve': [[...group, 'reservationId'], []],
  'gatt.ordered.marker.start': [[...group, 'handle', 'selector', 'valueB64', 'mode'], []]
}
for (const action of ['status', 'poll', 'poll-discard', 'ack-group']) ORDERED_SCHEMAS[`gatt.ordered.${action}`] = [group, []]
for (const action of ['ack-delivery', 'discard', 'ack-discard']) ORDERED_SCHEMAS[`gatt.ordered.${action}`] = [[...group, 'deliveryId'], []]
ORDERED_SCHEMAS['gatt.ordered.close'] = [[...group, 'operationId'], ['budgetMs']]
ORDERED_SCHEMAS['gatt.ordered.forget-group'] = [[...group, 'reclaimCapability'], []]
for (const action of ['status', 'cancel', 'forget']) ORDERED_SCHEMAS[`gatt.ordered.marker.${action}`] = [[...group, 'handle'], []]
for (const action of ['reserve', 'start', 'status', 'commit', 'cancel', 'release', 'retire', 'ack-retire', 'forget-retire', 'ack-forget']) {
  ORDERED_SCHEMAS[`gatt.ordered.recording-reclaim.${action}`] = [[...reclaim, 'requestId', 'callerKey'], []]
}

function validateOrderedArgs(op, args, invalid, WireFault, canonicalUuid) {
  const fail = key => { throw invalid(`args.${op}.${key}`) }
  const string = (obj, key, optional = false) => {
    if (optional && obj[key] == null) return
    if (typeof obj[key] !== 'string' || obj[key].length === 0) fail(key)
  }
  const integer = (obj, key, max, optional = false) => {
    if (optional && obj[key] == null) return
    if (!Number.isSafeInteger(obj[key]) || obj[key] < 0 || obj[key] > max) fail(key)
  }
  const decimal = (obj, key) => {
    string(obj, key)
    const value = obj[key]
    const maximum = '18446744073709551615'
    if (!/^(0|[1-9][0-9]*)$/.test(value) || value.length > maximum.length ||
      (value.length === maximum.length && value > maximum)) fail(key)
  }
  const choice = (obj, key, choices, optional = false) => {
    if (optional && obj[key] == null) return
    if (!choices.includes(obj[key])) fail(key)
  }
  const exact = (obj, required, optional = []) => {
    if (typeof obj !== 'object' || obj === null || Array.isArray(obj)) fail('object')
    for (const key of required) if (!(key in obj)) fail(key)
    for (const key of Object.keys(obj)) if (!required.includes(key) && !optional.includes(key)) fail(key)
  }
  const selector = obj => {
    const fields = ['serviceUuid', 'serviceOccurrence', 'characteristicUuid', 'characteristicOccurrence']
    exact(obj, fields)
    for (const key of ['serviceUuid', 'characteristicUuid']) {
      string(obj, key)
      canonicalUuid(obj[key])
    }
    for (const key of ['serviceOccurrence', 'characteristicOccurrence']) integer(obj, key, Number.MAX_SAFE_INTEGER)
  }
  if ('peerId' in args) string(args, 'peerId')
  for (const key of ['collectionId', 'deliveryId', 'handle', 'requestId']) if (key in args) decimal(args, key)
  for (const key of ['reclaimCapability', 'recordingId', 'callerKey', 'reservationId', 'operationId',
    'connectionGeneration', 'databaseGeneration', 'publicConnectionGeneration', 'publicDatabaseGeneration']) {
    if (key in args) string(args, key, (key === 'recordingId' && op === 'gatt.ordered.reclaim') || (key === 'callerKey' && op.endsWith('.next')))
  }
  if ('budgetMs' in args && args.budgetMs != null) {
    integer(args, 'budgetMs', 2147483647)
    if (args.budgetMs === 0) fail('budgetMs')
  }
  if (op === 'gatt.ordered.open') {
    integer(args, 'wireVersion', Number.MAX_SAFE_INTEGER)
    if (args.wireVersion !== 2) throw new WireFault('protocol.incompatible', 'core', 'gatt.ordered.version')
    if ('failureMetadataVersion' in args) {
      integer(args, 'failureMetadataVersion', Number.MAX_SAFE_INTEGER)
      if (args.failureMetadataVersion !== 1) throw new WireFault('protocol.incompatible', 'core', 'gatt.ordered.failure-metadata-version')
    }
    for (const key of ['maxItems', 'maxBytes', 'maxControls']) integer(args, key, 4294967295)
    if (!Array.isArray(args.members) || args.members.length > 128) fail('members')
    for (const member of args.members) {
      exact(member, ['memberId', 'publicMemberId', 'selector', 'requestedDelivery'], ['deliveryMode', 'preferredDeliveryMode'])
      decimal(member, 'memberId')
      string(member, 'publicMemberId')
      selector(member.selector)
      choice(member, 'requestedDelivery', ['prefer-notification', 'prefer-indication', 'require-notification', 'require-indication'])
      for (const key of ['deliveryMode', 'preferredDeliveryMode']) choice(member, key, ['notification', 'indication'], true)
    }
  }
  if (op === 'gatt.ordered.increase-capacity') {
    integer(args, 'maxBytes', 4194304)
    if (args.maxBytes === 0) fail('maxBytes')
    integer(args, 'wireVersion', Number.MAX_SAFE_INTEGER)
    if (args.wireVersion !== 1) fail('wireVersion')
    exact(args.sourceGeneration, ['kind', 'value'])
    choice(args.sourceGeneration, 'kind', ['counter', 'opaque'])
    if (args.sourceGeneration.kind === 'counter') decimal(args.sourceGeneration, 'value')
    else string(args.sourceGeneration, 'value')
  }
  if (op === 'gatt.ordered.marker.start') {
    selector(args.selector)
    choice(args, 'mode', ['with-response', 'without-response'])
    const value = args.valueB64
    if (typeof value !== 'string') fail('valueB64')
    if (value.length > 699052) throw new WireFault('bytes.too-large', 'core', `ubm-mobile.wire.args.${op}.valueB64`)
    const bytes = NodeBuffer.from(value, 'base64')
    if (bytes.toString('base64') !== value) fail('valueB64')
    if (bytes.length > 524288) throw new WireFault('bytes.too-large', 'core', `ubm-mobile.wire.args.${op}.valueB64`)
  }
}

module.exports = { ORDERED_SCHEMAS, validateOrderedArgs }
