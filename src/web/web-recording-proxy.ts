import type { WebProxyCrypto } from './web-recording-proxy-crypto'
import {
  BackendContractError,
  BLE_ERROR_CODES,
  BLE_ERROR_DOMAINS,
  optionalPlatformErrorDetail
} from '../backend-contract/errors'
import type { NormalizedBleError } from '../backend-contract/errors'
import {
  decode,
  directionalKey,
  encode,
  hex,
  mac,
  nonce,
  proxyError,
  unhex,
  verifyMac,
  WEB_PROXY_LIMIT,
  WEB_PROXY_VERSION
} from './web-recording-proxy-crypto'

/** A partition-local channel or an explicitly transferred MessagePort adapter. */
export interface WebRecordingProxyChannel {
  postMessage(message: string): void
  subscribe(listener: (message: unknown) => void): () => void
  close(): void
}
export interface WebRecordingProxyKeys {
  readonly crypto: WebProxyCrypto
  readonly channel: WebRecordingProxyChannel
  readonly secret: string
  readonly rendezvous: string
  readonly ownerChallenge: string
  readonly clientChallenge: string
}
type Envelope = { v: string; r: string; d: string; s: number; c: string; m: string }
function record(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}
function normalizedError(value: unknown): NormalizedBleError | undefined {
  if (!record(value)) return undefined
  const code = BLE_ERROR_CODES.find(item => item === value.code)
  const domain = BLE_ERROR_DOMAINS.find(item => item === value.domain)
  const { operation, retryability, commit } = value
  if (
    !code ||
    !domain ||
    typeof operation !== 'string' ||
    !operation ||
    operation.length > 256 ||
    (retryability !== 'never' && retryability !== 'caller-decides') ||
    (commit !== undefined && commit !== null && commit !== 'uncertain' && commit !== 'not-dispatched') ||
    !(value.platform === null || record(value.platform))
  )
    return undefined
  try {
    const platform = optionalPlatformErrorDetail(value.platform, 'web proxy') ?? null
    return { code, domain, operation, retryability, platform, ...(commit === undefined ? {} : { commit }) }
  } catch {
    return undefined
  }
}
function ownerError(error: unknown): string {
  try {
    const source = record(error) && 'normalized' in error ? error.normalized : error
    const normalized = normalizedError(source)
    if (normalized) {
      const wire = JSON.stringify({ kind: 'normalized', normalized })
      if (encode(wire).length <= 8192) return wire
      return JSON.stringify({ kind: 'error-detail-too-large', outcome: 'failed' })
    }
  } catch {
    // An unrepresentable exception cannot safely cross this boundary.
  }
  return JSON.stringify({ kind: 'owner-operation-failed' })
}
function remoteError(value: string): Error {
  try {
    const parsed: unknown = JSON.parse(value)
    if (record(parsed)) {
      if (parsed.kind === 'normalized') {
        const normalized = normalizedError(parsed.normalized)
        if (normalized) return new BackendContractError(normalized)
      }
      if (parsed.kind === 'result-too-large' && parsed.outcome === 'completed')
        return Object.assign(proxyError('result-too-large'), { outcome: 'completed' })
      if (parsed.kind === 'error-detail-too-large' && parsed.outcome === 'failed')
        return Object.assign(proxyError('error-detail-too-large'), { outcome: 'failed' })
      if (parsed.kind === 'owner-operation-failed') return proxyError('owner-operation-failed')
    }
  } catch {
    // Local transport failures use a plain code.
  }
  return proxyError(value)
}
function envelope(value: unknown): value is Envelope {
  if (typeof value !== 'object' || value === null) return false
  return (
    'v' in value &&
    'r' in value &&
    'd' in value &&
    's' in value &&
    'c' in value &&
    'm' in value &&
    typeof value.v === 'string' &&
    typeof value.r === 'string' &&
    typeof value.d === 'string' &&
    typeof value.s === 'number' &&
    Number.isSafeInteger(value.s) &&
    value.s > 0 &&
    typeof value.c === 'string' &&
    value.c.length <= (WEB_PROXY_LIMIT + 16) * 2 &&
    typeof value.m === 'string'
  )
}
export async function createWebRecordingProxy(
  options: WebRecordingProxyKeys & {
    readonly role: 'owner' | 'client'
    /** Owner callback must retain and serialize the real graph and cleanup obligations. */
    readonly execute?: (payload: string) => Promise<string>
    readonly onRelease?: () => void
    /** Retain and reconcile owner obligations when this transport ends. */
    readonly onClose?: () => void
  }
): Promise<{
  request(id: string, payload: string, timeoutMs: number): Promise<string>
  retire(id: string): void
  release(timeoutMs?: number): Promise<void>
  revoke(): Promise<void>
  close(): void
}> {
  const { crypto, channel, secret, rendezvous, ownerChallenge, clientChallenge, role } = options
  const sendDirection = role === 'client' ? 'client' : 'owner'
  const receiveDirection = role === 'client' ? 'owner' : 'client'
  const sendKey = await directionalKey(crypto, secret, ownerChallenge, clientChallenge, sendDirection)
  const receiveKey = await directionalKey(crypto, secret, ownerChallenge, clientChallenge, receiveDirection)
  const prefix = [WEB_PROXY_VERSION, rendezvous, ownerChallenge, clientChallenge]
  let closed = false
  let releasing = false
  let revoking = false
  let releaseResult: { error: Error | null } | undefined
  const releaseWaiters = new Set<(result: { error: Error | null }) => void>()
  const settleRelease = (error: Error | null): void => {
    if (releaseResult) return
    releaseResult = { error }
    for (const waiter of releaseWaiters) waiter(releaseResult)
  }
  let queued = 0
  let sendSeq = 0
  let receiveSeq = 0
  let lastIncoming = ''
  let lastReply = ''
  // A retry can arrive while its owner request is queued or executing. This
  // remains bounded by `enqueued` (at most 32 distinct raw frames) and records
  // one replay demand per authenticated request without another execution.
  const deferredOwnerReplays = new Set<string>()
  let acknowledged = 0
  let releaseSending = false
  let work = Promise.resolve()
  const enqueued = new Set<string>()
  let pending:
    | {
        id: string
        payload: string
        wire: Promise<string>
        wireValue?: string
        result?: { ok: boolean; value: string }
        waiters: Set<(r: { ok: boolean; value: string }) => void>
      }
    | undefined
  const close = (): void => {
    if (closed) return
    closed = true
    deferredOwnerReplays.clear()
    unsubscribe()
    channel.close()
    settleRelease(proxyError('closed'))
    if (pending) for (const done of pending.waiters) done({ ok: false, value: 'closed' })
    options.onClose?.()
  }
  const seal = async (payload: string): Promise<string> => {
    if (encode(payload).length > WEB_PROXY_LIMIT) throw proxyError('size')
    if (sendSeq >= Number.MAX_SAFE_INTEGER) throw proxyError('sequence-exhausted')
    const sequence = ++sendSeq
    const aad = [...prefix, sendDirection, String(sequence)]
    const encrypted = await crypto.subtle.encrypt(
      { name: 'AES-GCM', iv: nonce(sequence), additionalData: encode(JSON.stringify(aad)) },
      sendKey,
      encode(payload)
    )
    const ciphertext = hex(new Uint8Array(encrypted))
    const signature = await mac(crypto, secret, [...aad, ciphertext])
    return JSON.stringify({
      v: WEB_PROXY_VERSION,
      r: rendezvous,
      d: sendDirection,
      s: sequence,
      c: ciphertext,
      m: signature
    })
  }
  const receive = async (raw: unknown): Promise<void> => {
    if (closed || revoking || typeof raw !== 'string' || raw.length > (WEB_PROXY_LIMIT + 16) * 2 + 1024) return
    let parsed: unknown
    try {
      parsed = JSON.parse(raw)
    } catch {
      return
    }
    if (
      typeof parsed === 'object' &&
      parsed !== null &&
      'v' in parsed &&
      parsed.v === WEB_PROXY_VERSION &&
      'r' in parsed &&
      parsed.r === rendezvous &&
      'control' in parsed &&
      'direction' in parsed &&
      'signature' in parsed &&
      typeof parsed.control === 'string' &&
      typeof parsed.direction === 'string' &&
      typeof parsed.signature === 'string'
    ) {
      const action = parsed.control
      if (
        parsed.direction !== receiveDirection ||
        (role === 'owner' ? !/^release:(0|[1-9][0-9]*)$/.test(action) : action !== 'release-ack' && action !== 'revoke')
      )
        return
      if (
        !(await verifyMac(crypto, secret, [...prefix, 'control', receiveDirection, '0', action], parsed.signature)) ||
        closed
      )
        return
      if (role === 'owner') {
        if (action !== `release:${receiveSeq}` || (receiveSeq > 0 && !lastReply)) return
        options.onRelease?.()
        const signature = await mac(crypto, secret, [...prefix, 'control', sendDirection, '0', 'release-ack'])
        if (!closed)
          channel.postMessage(
            JSON.stringify({
              v: WEB_PROXY_VERSION,
              r: rendezvous,
              control: 'release-ack',
              direction: sendDirection,
              signature
            })
          )
        close()
      } else if (action === 'revoke') {
        close()
      } else if (releasing) {
        settleRelease(null)
        close()
      }
      return
    }
    if (!envelope(parsed) || parsed.v !== WEB_PROXY_VERSION || parsed.r !== rendezvous || parsed.d !== receiveDirection)
      return
    const aad = [...prefix, parsed.d, String(parsed.s)]
    if (!(await verifyMac(crypto, secret, [...aad, parsed.c], parsed.m))) return
    if (closed) return
    if (parsed.s === receiveSeq) {
      if (raw !== lastIncoming) {
        close()
        return
      }
      if (role === 'owner' && lastReply) {
        const replay = deferredOwnerReplays.delete(raw)
        channel.postMessage(lastReply)
        if (replay) channel.postMessage(lastReply)
      }
      return
    }
    if (parsed.s !== receiveSeq + 1) {
      close()
      return
    }
    let plaintext: string
    try {
      const bytes = unhex(parsed.c, parsed.c.length / 2)
      plaintext = decode(
        await crypto.subtle.decrypt(
          { name: 'AES-GCM', iv: nonce(parsed.s), additionalData: encode(JSON.stringify(aad)) },
          receiveKey,
          bytes
        )
      )
    } catch {
      close()
      return
    }
    if (closed) return
    let body: unknown
    try {
      body = JSON.parse(plaintext)
    } catch {
      close()
      return
    }
    if (
      !Array.isArray(body) ||
      typeof body[0] !== 'string' ||
      body[0].length === 0 ||
      body[0].length > 256 ||
      typeof body[1] !== 'string' ||
      (role === 'owner'
        ? body.length !== 3 || body[2] !== String(receiveSeq)
        : body.length !== 3 || typeof body[2] !== 'string' || (body[1] !== 'ok' && body[1] !== 'error'))
    ) {
      close()
      return
    }
    receiveSeq = parsed.s
    lastIncoming = raw
    if (role === 'owner') {
      if (!options.execute || body[0] !== String(receiveSeq)) {
        close()
        return
      }
      lastReply = ''
      let result: string
      try {
        const value = await options.execute(body[1])
        result = JSON.stringify([body[0], 'ok', value])
        if (encode(result).length > WEB_PROXY_LIMIT)
          result = JSON.stringify([
            body[0],
            'error',
            JSON.stringify({ kind: 'result-too-large', outcome: 'completed' })
          ])
      } catch (error) {
        result = JSON.stringify([body[0], 'error', ownerError(error)])
      }
      if (closed) return
      try {
        lastReply = await seal(result)
        const replay = deferredOwnerReplays.delete(raw)
        if (!closed) {
          channel.postMessage(lastReply)
          if (replay) channel.postMessage(lastReply)
        }
      } catch {
        close()
      }
    } else {
      if (!pending || pending.id !== body[0]) {
        close()
        return
      }
      const result = { ok: body[1] === 'ok', value: body[2] }
      pending.result = result
      for (const done of pending.waiters) done(result)
    }
  }
  // Serialize crypto and execution to keep arrival order and duplicate identity exact.
  const unsubscribe = channel.subscribe(raw => {
    if (closed || typeof raw !== 'string' || raw.length > (WEB_PROXY_LIMIT + 16) * 2 + 1024) return
    let route: unknown
    try {
      route = JSON.parse(raw)
    } catch {
      return
    }
    if (
      typeof route !== 'object' ||
      route === null ||
      !('v' in route && route.v === WEB_PROXY_VERSION && 'r' in route && route.r === rendezvous) ||
      (!('d' in route && route.d === receiveDirection) &&
        !('direction' in route && route.direction === receiveDirection))
    )
      return
    if (enqueued.has(raw)) {
      if (role === 'owner') deferredOwnerReplays.add(raw)
      return
    }
    if (role === 'owner' && raw === lastIncoming && !lastReply) {
      deferredOwnerReplays.add(raw)
      return
    }
    if (queued >= 32) return
    queued++
    enqueued.add(raw)
    work = work
      .then(() => receive(raw))
      .catch(() => close())
      .finally(() => {
        queued--
        enqueued.delete(raw)
        // A duplicate can arrive after `receive` sent its reply but before
        // this queue entry retires. It still gets one authenticated cached
        // response; malformed or failed work simply releases its marker.
        const replay = deferredOwnerReplays.delete(raw)
        if (replay && role === 'owner' && raw === lastIncoming && lastReply && !closed) {
          try {
            channel.postMessage(lastReply)
          } catch {
            close()
          }
        }
      })
  })
  const sendRelease = (): void => {
    if (!releasing || pending || closed || releaseSending || releaseResult) return
    releaseSending = true
    const action = `release:${acknowledged}`
    mac(crypto, secret, [...prefix, 'control', sendDirection, '0', action])
      .then(signature => {
        if (!closed)
          channel.postMessage(
            JSON.stringify({
              v: WEB_PROXY_VERSION,
              r: rendezvous,
              control: action,
              direction: sendDirection,
              signature
            })
          )
      })
      .catch(error => settleRelease(error instanceof Error ? error : proxyError('protocol')))
      .finally(() => {
        releaseSending = false
      })
  }
  return {
    async request(id, payload, timeoutMs) {
      if (role !== 'client' || closed || (releasing && (!pending || pending.id !== id || pending.payload !== payload)))
        throw proxyError('closed')
      if (!id || id.length > 256 || !Number.isFinite(timeoutMs) || timeoutMs <= 0) throw proxyError('argument')
      if (pending && (pending.id !== id || pending.payload !== payload)) throw proxyError('pending')
      if (!pending) {
        if (id !== String(sendSeq + 1)) throw proxyError('identity')
        const body = JSON.stringify([id, payload, String(acknowledged)])
        if (encode(body).length > WEB_PROXY_LIMIT) throw proxyError('size')
        pending = { id, payload, wire: seal(body), waiters: new Set() }
        const admitted = pending
        admitted.wire
          .then(wire => {
            admitted.wireValue = wire
            if (!closed) channel.postMessage(wire)
          })
          .catch(() => close())
      } else if (pending.wireValue && !pending.result) {
        try {
          channel.postMessage(pending.wireValue)
        } catch {
          close()
          throw proxyError('closed')
        }
      }
      const current = pending
      if (current.result) {
        if (!current.result.ok) throw remoteError(current.result.value)
        return current.result.value
      }
      if (current.waiters.size >= 32) throw proxyError('capacity')
      return new Promise((resolve, reject) => {
        const done = (result: { ok: boolean; value: string }): void => {
          clearTimeout(timer)
          current.waiters.delete(done)
          if (result.ok) resolve(result.value)
          else reject(remoteError(result.value))
        }
        const timer = setTimeout(() => {
          current.waiters.delete(done)
          reject(proxyError('timeout'))
        }, timeoutMs)
        current.waiters.add(done)
      })
    },
    async revoke() {
      if (role !== 'owner' || closed || revoking) throw proxyError('closed')
      revoking = true
      try {
        const signature = await mac(crypto, secret, [...prefix, 'control', sendDirection, '0', 'revoke'])
        if (!closed)
          channel.postMessage(
            JSON.stringify({
              v: WEB_PROXY_VERSION,
              r: rendezvous,
              control: 'revoke',
              direction: sendDirection,
              signature
            })
          )
      } finally {
        close()
      }
    },
    async release(timeoutMs = 1000) {
      if (role !== 'client' || closed) throw proxyError('closed')
      if (!Number.isFinite(timeoutMs) || timeoutMs <= 0) throw proxyError('argument')
      if (!releasing) {
        releasing = true
      }
      sendRelease()
      if (releaseResult) {
        if (releaseResult.error) throw releaseResult.error
        return
      }
      if (releaseWaiters.size >= 32) throw proxyError('capacity')
      return new Promise<void>((resolve, reject) => {
        const done = (result: { error: Error | null }): void => {
          clearTimeout(timer)
          releaseWaiters.delete(done)
          if (result.error) reject(result.error)
          else resolve()
        }
        const timer = setTimeout(() => {
          releaseWaiters.delete(done)
          reject(proxyError('timeout'))
        }, timeoutMs)
        releaseWaiters.add(done)
      })
    },
    retire(id) {
      if (!pending || pending.id !== id || !pending.result) throw proxyError('pending')
      acknowledged = Number(id)
      pending = undefined
      sendRelease()
    },
    close
  }
}
