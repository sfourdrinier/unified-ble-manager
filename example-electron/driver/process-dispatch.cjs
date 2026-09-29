'use strict'

const { encodeNativeContinuationFailure } = require('unified-ble-manager/electron/main')

// Application routing only: native codecs remain the authority for declaration,
// identity and token semantics. Validate shape before acquiring any owner.
const COMMANDS = Object.freeze({
  execute: ['execute', ['peerId', 'declarationJson']],
  status: ['describeBacklog', []],
  'prepare-claim': ['prepareClaim', ['maxItems', 'maxBytes']],
  'acknowledge-claim': ['acknowledgeClaim', ['token']],
  'recording-status': ['status', ['id']],
  'recording-prepare': ['prepare', ['id', 'maxItems', 'maxBytes']],
  'recording-acknowledge': ['acknowledge', ['id', 'token']],
  'recording-stop': ['stop', ['id']],
  'recording-clear': ['clear', ['id']]
})

function createProcessDispatch({ controls, recordings }) {
  return async request => {
    const fail = () => {
      throw new Error('process bridge arguments refused')
    }
    if (
      request === null ||
      typeof request !== 'object' ||
      typeof request.operation !== 'string' ||
      !Object.hasOwn(COMMANDS, request.operation)
    )
      fail()
    const [method, fields] = COMMANDS[request.operation]
    const args = request.args
    if (
      args === null ||
      typeof args !== 'object' ||
      Array.isArray(args) ||
      Object.keys(args).length !== fields.length ||
      fields.some(field => !Object.hasOwn(args, field))
    )
      fail()
    for (const field of fields) {
      const value = args[field]
      if (field === 'maxItems' || field === 'maxBytes') {
        // Reject before N-API's u32 conversion; native policy applies its
        // tighter per-operation bounds without a JavaScript truncation first.
        if (!Number.isSafeInteger(value) || value <= 0 || value > 0xffffffff) fail()
      } else if (typeof value !== 'string' || value.length === 0) fail()
    }
    try {
      const access = await (request.operation.startsWith('recording-') ? recordings() : controls())
      // Preserve the receiver and canonical native envelope. In particular, never
      // turn prepare into a main-side claim, which would ACK before IPC delivery.
      return await access[method](...fields.map(field => args[field]))
    } catch (error) {
      // Electron serializes rejected Error objects as strings. Preserve only
      // genuine, native-compatible typed failures inside the existing envelope.
      // Authentication and argument validation remain outside this conversion.
      return encodeNativeContinuationFailure(error)
    }
  }
}

module.exports = { createProcessDispatch }
