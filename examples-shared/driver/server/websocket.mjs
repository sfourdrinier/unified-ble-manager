// examples-shared/driver/server/websocket.mjs
//
// Minimal RFC 6455 server side (text frames, fragmentation, ping/pong, close)
// so the host driver needs no npm dependency. Binary frames are refused: the
// driver protocol is JSON text only.

import { createHash } from 'node:crypto'
import { EventEmitter } from 'node:events'

const HANDSHAKE_GUID = '258EAFA5-E914-47DA-95CA-C5AB0DC85B11'
const OPCODE = { continuation: 0x0, text: 0x1, binary: 0x2, close: 0x8, ping: 0x9, pong: 0xa }
const DEFAULT_MAX_MESSAGE_BYTES = 16 * 1024 * 1024
const utf8 = new TextDecoder('utf-8', { fatal: true })

/** Completes the upgrade or answers 400; returns null when refused. */
export function acceptWebSocket(request, socket, head, { maxMessageBytes = DEFAULT_MAX_MESSAGE_BYTES } = {}) {
  const key = request.headers['sec-websocket-key']
  const upgrade = String(request.headers.upgrade ?? '').toLowerCase()
  if (upgrade !== 'websocket' || typeof key !== 'string' || request.headers['sec-websocket-version'] !== '13') {
    socket.end('HTTP/1.1 400 Bad Request\r\nConnection: close\r\nContent-Type: text/plain\r\n\r\nexpected a WebSocket v13 upgrade\n')
    return null
  }
  const accept = createHash('sha1').update(key + HANDSHAKE_GUID).digest('base64')
  socket.write(
    'HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n' + `Sec-WebSocket-Accept: ${accept}\r\n\r\n`
  )
  socket.setNoDelay(true)
  const connection = new WebSocketConnection(socket, maxMessageBytes)
  if (head !== undefined && head.length > 0) connection.receive(head)
  return connection
}

export function encodeFrame(opcode, payload, { fin = true, mask = false } = {}) {
  const length = payload.length
  const headerLength = 2 + (length < 126 ? 0 : length < 65_536 ? 2 : 8) + (mask ? 4 : 0)
  const frame = Buffer.alloc(headerLength + length)
  frame[0] = (fin ? 0x80 : 0) | opcode
  let offset = 2
  if (length < 126) {
    frame[1] = length
  } else if (length < 65_536) {
    frame[1] = 126
    frame.writeUInt16BE(length, 2)
    offset = 4
  } else {
    frame[1] = 127
    frame.writeBigUInt64BE(BigInt(length), 2)
    offset = 10
  }
  if (mask) {
    frame[1] |= 0x80
    const key = Buffer.from([0x12, 0x34, 0x56, 0x78])
    key.copy(frame, offset)
    offset += 4
    for (let index = 0; index < length; index += 1) frame[offset + index] = payload[index] ^ key[index % 4]
  } else {
    payload.copy(frame, offset)
  }
  return frame
}

export class WebSocketConnection extends EventEmitter {
  #socket
  #maxMessageBytes
  #buffer = Buffer.alloc(0)
  #fragments = []
  #fragmentBytes = 0
  #fragmentOpcode = null
  #closeSent = false
  #closed = false

  constructor(socket, maxMessageBytes) {
    super()
    this.#socket = socket
    this.#maxMessageBytes = maxMessageBytes
    socket.on('data', chunk => this.receive(chunk))
    socket.on('error', error => this.emit('error', error))
    socket.on('close', () => this.#finish(1006, 'socket closed without a close frame'))
  }

  get open() {
    return !this.#closed && !this.#closeSent
  }

  send(text) {
    if (!this.open) throw new Error('WebSocket is not open')
    this.#socket.write(encodeFrame(OPCODE.text, Buffer.from(text, 'utf8')))
  }

  ping() {
    if (this.open) this.#socket.write(encodeFrame(OPCODE.ping, Buffer.alloc(0)))
  }

  close(code = 1000, reason = '') {
    if (this.#closeSent || this.#closed) return
    this.#closeSent = true
    const reasonBytes = Buffer.from(reason, 'utf8').subarray(0, 123)
    const payload = Buffer.alloc(2 + reasonBytes.length)
    payload.writeUInt16BE(code, 0)
    reasonBytes.copy(payload, 2)
    this.#socket.write(encodeFrame(OPCODE.close, payload))
    this.#socket.end()
    this.#finish(code, reason)
  }

  terminate(code = 1006, reason = 'terminated') {
    this.#socket.destroy()
    this.#finish(code, reason)
  }

  receive(chunk) {
    this.#buffer = this.#buffer.length === 0 ? chunk : Buffer.concat([this.#buffer, chunk])
    while (!this.#closed) {
      const frame = this.#readFrame()
      if (frame === null) return
      this.#handleFrame(frame)
    }
  }

  #readFrame() {
    const buffer = this.#buffer
    if (buffer.length < 2) return null
    const fin = (buffer[0] & 0x80) !== 0
    const opcode = buffer[0] & 0x0f
    const masked = (buffer[1] & 0x80) !== 0
    let length = buffer[1] & 0x7f
    let offset = 2
    if (length === 126) {
      if (buffer.length < 4) return null
      length = buffer.readUInt16BE(2)
      offset = 4
    } else if (length === 127) {
      if (buffer.length < 10) return null
      const wide = buffer.readBigUInt64BE(2)
      if (wide > BigInt(this.#maxMessageBytes)) {
        this.#fail(1009, 'frame exceeds the message size limit')
        return null
      }
      length = Number(wide)
      offset = 10
    }
    if (!masked) {
      this.#fail(1002, 'client frames must be masked')
      return null
    }
    if (buffer.length < offset + 4 + length) return null
    const key = buffer.subarray(offset, offset + 4)
    const payload = Buffer.alloc(length)
    for (let index = 0; index < length; index += 1) payload[index] = buffer[offset + 4 + index] ^ key[index % 4]
    this.#buffer = buffer.subarray(offset + 4 + length)
    return { fin, opcode, payload }
  }

  #handleFrame({ fin, opcode, payload }) {
    switch (opcode) {
      case OPCODE.ping:
        this.#socket.write(encodeFrame(OPCODE.pong, payload))
        return
      case OPCODE.pong:
        this.emit('pong')
        return
      case OPCODE.close: {
        const code = payload.length >= 2 ? payload.readUInt16BE(0) : 1005
        const reason = payload.length > 2 ? payload.subarray(2).toString('utf8') : ''
        if (!this.#closeSent) {
          this.#closeSent = true
          this.#socket.write(encodeFrame(OPCODE.close, payload.subarray(0, 2)))
          this.#socket.end()
        }
        this.#finish(code, reason)
        return
      }
      case OPCODE.binary:
        this.#fail(1003, 'binary frames are not part of the driver protocol')
        return
      case OPCODE.text:
      case OPCODE.continuation:
        this.#collect(fin, opcode, payload)
        return
      default:
        this.#fail(1002, `unknown opcode ${opcode}`)
    }
  }

  #collect(fin, opcode, payload) {
    if (opcode === OPCODE.continuation && this.#fragmentOpcode === null) {
      this.#fail(1002, 'continuation frame without a message in progress')
      return
    }
    if (opcode === OPCODE.text && this.#fragmentOpcode !== null) {
      this.#fail(1002, 'new message started before the previous one finished')
      return
    }
    if (opcode === OPCODE.text) this.#fragmentOpcode = opcode
    this.#fragments.push(payload)
    this.#fragmentBytes += payload.length
    if (this.#fragmentBytes > this.#maxMessageBytes) {
      this.#fail(1009, 'message exceeds the size limit')
      return
    }
    if (!fin) return
    const message = Buffer.concat(this.#fragments)
    this.#fragments = []
    this.#fragmentBytes = 0
    this.#fragmentOpcode = null
    let text
    try {
      text = utf8.decode(message)
    } catch {
      this.#fail(1007, 'text message is not valid UTF-8')
      return
    }
    this.emit('message', text)
  }

  #fail(code, reason) {
    this.emit('protocol-error', { code, reason })
    this.close(code, reason)
  }

  #finish(code, reason) {
    if (this.#closed) return
    this.#closed = true
    this.emit('close', { code, reason })
  }
}
