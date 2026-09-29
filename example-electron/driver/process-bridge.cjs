'use strict'

const PROCESS_CHANNEL = 'ubm-reference-process/1'
const OPERATIONS = new Set([
  'execute',
  'status',
  'prepare-claim',
  'acknowledge-claim',
  'recording-status',
  'recording-prepare',
  'recording-acknowledge',
  'recording-stop',
  'recording-clear'
])
const REQUEST_BYTES = 512 * 1024
const RESPONSE_BYTES = 8 * 1024 * 1024

function documentIdentity(value) {
  try {
    const url = new URL(value)
    url.search = ''
    url.hash = ''
    return url.href
  } catch {
    return null
  }
}

function bounded(value, maximum) {
  const encoded = JSON.stringify(value)
  if (encoded === undefined || Buffer.byteLength(encoded) > maximum)
    throw new Error('process bridge message size refused')
}

/** Application control only. The dispatcher uses canonical native envelopes;
 * preparation never acknowledges a volatile or durable prefix. */
function installAuthenticatedAppHandler({ ipcMain, window, documentUrl, channel, dispatch, validateRequest }) {
  const sender = window.webContents
  const expected = documentIdentity(documentUrl)
  if (expected === null || !expected.startsWith('file:'))
    throw new Error('process bridge requires a local app document')
  let epoch = 0
  let closed = false
  let navigating = false
  let outstanding = 0
  const retire = () => {
    epoch += 1
    navigating = true
  }
  const navigationStarted = details => {
    if (details.isMainFrame && !details.isSameDocument) retire()
  }
  const loaded = () => {
    navigating = false
  }
  sender.on('did-start-navigation', navigationStarted)
  sender.on('render-process-gone', retire)
  sender.on('destroyed', retire)
  sender.on('did-finish-load', loaded)
  function authenticate(event) {
    if (closed) throw new Error('process bridge closed')
    const frame = sender.mainFrame
    if (
      navigating ||
      sender.isDestroyed() ||
      event.sender !== sender ||
      event.processId !== frame.processId ||
      event.frameId !== frame.routingId ||
      event.senderFrame == null ||
      documentIdentity(event.senderFrame.url) !== expected ||
      documentIdentity(frame.url) !== expected
    )
      throw new Error('process bridge unauthorized frame')
  }
  ipcMain.handle(channel, async (event, request) => {
    authenticate(event)
    bounded(request, REQUEST_BYTES)
    validateRequest(request)
    if (outstanding >= 8) throw new Error('process bridge outstanding request limit')
    const admittedEpoch = epoch
    outstanding += 1
    try {
      const response = await dispatch(request, event)
      if (admittedEpoch !== epoch) throw new Error('process bridge frame retired')
      authenticate(event)
      bounded(response, RESPONSE_BYTES)
      return response
    } finally {
      outstanding -= 1
    }
  })
  return () => {
    closed = true
    epoch += 1
    ipcMain.removeHandler(channel)
    sender.removeListener('did-start-navigation', navigationStarted)
    sender.removeListener('render-process-gone', retire)
    sender.removeListener('destroyed', retire)
    sender.removeListener('did-finish-load', loaded)
  }
}

function installProcessBridge(options) {
  return installAuthenticatedAppHandler({
    ...options,
    channel: PROCESS_CHANNEL,
    validateRequest(request) {
      if (
        request === null ||
        typeof request !== 'object' ||
        Array.isArray(request) ||
        Object.keys(request).length !== 2 ||
        !Object.hasOwn(request, 'operation') ||
        !Object.hasOwn(request, 'args') ||
        !OPERATIONS.has(request.operation) ||
        request.args === null ||
        typeof request.args !== 'object' ||
        Array.isArray(request.args)
      ) {
        throw new Error('process bridge command refused')
      }
    }
  })
}

module.exports = { PROCESS_CHANNEL, installProcessBridge, installAuthenticatedAppHandler }
