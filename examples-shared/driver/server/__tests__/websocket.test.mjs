import { test } from 'node:test'
import assert from 'node:assert/strict'
import { createServer } from 'node:http'
import { connect } from 'node:net'
import { once } from 'node:events'
import { acceptWebSocket, encodeFrame } from '../websocket.mjs'

async function withServer(onConnection, run) {
  const server = createServer()
  server.on('upgrade', (request, socket, head) => {
    const connection = acceptWebSocket(request, socket, head)
    if (connection !== null) onConnection(connection, request)
  })
  server.listen(0, '127.0.0.1')
  await once(server, 'listening')
  const { port } = server.address()
  try {
    await run(port)
  } finally {
    server.closeAllConnections()
    server.close()
  }
}

function echo(connection) {
  connection.on('message', text => connection.send(text))
}

async function openClient(port, path = '/host') {
  const client = new WebSocket(`ws://127.0.0.1:${port}${path}`)
  await once(client, 'open')
  return client
}

test('handshake and text round trip for small, 16-bit and 64-bit length payloads', async () => {
  await withServer(echo, async port => {
    const client = await openClient(port)
    for (const size of [5, 300, 70_000]) {
      const text = 'x'.repeat(size - 1) + 'é'
      client.send(text)
      const [event] = await once(client, 'message')
      assert.equal(event.data, text)
    }
    client.close()
  })
})

test('close from the client is reported with its code and reason', async () => {
  let closed
  await withServer(
    connection => connection.on('close', info => (closed = info)),
    async port => {
      const client = await openClient(port)
      client.close(4001, 'bye')
      await once(client, 'close')
      await new Promise(resolve => setTimeout(resolve, 20))
      assert.deepEqual(closed, { code: 4001, reason: 'bye' })
    }
  )
})

test('server close reaches the client with code and reason', async () => {
  await withServer(
    connection => connection.close(4403, 'protocol.version-mismatch'),
    async port => {
      const client = new WebSocket(`ws://127.0.0.1:${port}/host`)
      const [event] = await once(client, 'close')
      assert.equal(event.code, 4403)
      assert.equal(event.reason, 'protocol.version-mismatch')
    }
  )
})

test('ping gets a pong from a compliant client', async () => {
  let pongs = 0
  await withServer(
    connection => {
      connection.on('pong', () => (pongs += 1))
      connection.ping()
    },
    async port => {
      const client = await openClient(port)
      await new Promise(resolve => setTimeout(resolve, 50))
      assert.equal(pongs, 1)
      client.close()
    }
  )
})

function rawHandshake(port) {
  const socket = connect(port, '127.0.0.1')
  socket.write(
    'GET /host HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n' +
      'Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n'
  )
  return socket
}

async function readUntil(socket, predicate) {
  let buffer = Buffer.alloc(0)
  while (!predicate(buffer)) {
    const [chunk] = await once(socket, 'data')
    buffer = Buffer.concat([buffer, chunk])
  }
  return buffer
}

test('RFC 6455 sample key yields the RFC accept value and fragmented masked frames are reassembled', async () => {
  const received = []
  await withServer(
    connection => connection.on('message', text => received.push(text)),
    async port => {
      const socket = rawHandshake(port)
      const response = (await readUntil(socket, buffer => buffer.includes('\r\n\r\n'))).toString()
      assert.match(response, /^HTTP\/1\.1 101/)
      assert.match(response, /Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK\+xOo=/i)
      socket.write(Buffer.concat([encodeFrame(0x1, Buffer.from('hel'), { fin: false, mask: true }), encodeFrame(0x0, Buffer.from('lo'), { fin: true, mask: true })]))
      await new Promise(resolve => setTimeout(resolve, 30))
      assert.deepEqual(received, ['hello'])
      socket.destroy()
    }
  )
})

test('an unmasked client frame is a protocol error closed with 1002', async () => {
  let closed
  await withServer(
    connection => connection.on('close', info => (closed = info)),
    async port => {
      const socket = rawHandshake(port)
      await readUntil(socket, buffer => buffer.includes('\r\n\r\n'))
      socket.write(encodeFrame(0x1, Buffer.from('nope'), { fin: true, mask: false }))
      await once(socket, 'close')
      assert.equal(closed.code, 1002)
    }
  )
})

test('a request without a websocket key is refused with 400', async () => {
  await withServer(
    () => assert.fail('must not accept'),
    async port => {
      const socket = connect(port, '127.0.0.1')
      socket.write('GET /host HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n')
      const response = (await readUntil(socket, buffer => buffer.includes('\r\n\r\n'))).toString()
      assert.match(response, /^HTTP\/1\.1 400/)
      socket.destroy()
    }
  )
})
