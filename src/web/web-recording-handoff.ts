import type { WebProxyCrypto } from './web-recording-proxy-crypto'
import { mac, proxyError, randomHex, unhex, verifyMac, WEB_PROXY_VERSION } from './web-recording-proxy-crypto'
import { createWebRecordingProxy } from './web-recording-proxy'
import type { WebRecordingProxyChannel } from './web-recording-proxy'

/** Explicit capability: transfer only to a trusted tab. Never persist or log it. */
export interface WebRecordingHandoffTicket {
  readonly version: string
  readonly secret: string
  readonly rendezvous: string
  readonly incarnation: string
  /** Informational only: the issuing owner's monotonic clock controls expiry. */
  readonly lifetimeMs: number
  /** Meaningful only in the issuing owner's monotonic clock domain. */
  readonly expiresAtMonotonic: number
}
type Packet = { v: string; r: string; i: string; t: string; c: string; o: string; m: string }
function parse(raw: unknown): Packet | undefined {
  if (typeof raw !== 'string' || raw.length > 2048) return undefined
  try {
    const p: unknown = JSON.parse(raw)
    if (typeof p !== 'object' || p === null) return undefined
    if (!('v' in p && 'r' in p && 'i' in p && 't' in p && 'c' in p && 'o' in p && 'm' in p)) return undefined
    if (
      typeof p.v !== 'string' ||
      typeof p.r !== 'string' ||
      typeof p.i !== 'string' ||
      typeof p.t !== 'string' ||
      typeof p.c !== 'string' ||
      typeof p.o !== 'string' ||
      typeof p.m !== 'string'
    )
      return undefined
    return { v: p.v, r: p.r, i: p.i, t: p.t, c: p.c, o: p.o, m: p.m }
  } catch {
    return undefined
  }
}
function fields(ticket: WebRecordingHandoffTicket, type: string, client: string, owner: string): string[] {
  return [WEB_PROXY_VERSION, ticket.rendezvous, ticket.incarnation, type, client, owner, '0']
}
async function send(
  crypto: WebProxyCrypto,
  channel: WebRecordingProxyChannel,
  ticket: WebRecordingHandoffTicket,
  type: string,
  client: string,
  owner: string,
  canSend?: () => boolean
): Promise<boolean> {
  const signature = await mac(crypto, ticket.secret, fields(ticket, type, client, owner))
  if (canSend && !canSend()) return false
  channel.postMessage(
    JSON.stringify({
      v: WEB_PROXY_VERSION,
      r: ticket.rendezvous,
      i: ticket.incarnation,
      t: type,
      c: client,
      o: owner,
      m: signature
    })
  )
  return true
}
function matches(p: Packet, ticket: WebRecordingHandoffTicket): boolean {
  return p.v === WEB_PROXY_VERSION && p.r === ticket.rendezvous && p.i === ticket.incarnation
}
function validRandom(s: string): boolean {
  try {
    unhex(s, 32)
    return true
  } catch {
    return false
  }
}
export async function createWebRecordingHandoffOwner<Graph extends object>(options: {
  readonly crypto: WebProxyCrypto
  readonly channel: WebRecordingProxyChannel
  readonly lifetimeMs: number
  readonly now?: () => number
  /** The original owner graph object, retained locally and never transferred. */
  readonly graph: Graph
  readonly execute: (graph: Graph, payload: string) => Promise<string>
  /** Begin owner-side reconciliation; acknowledgement does not assert cleanup completion. */
  readonly onRelease?: () => void
  readonly onClose?: () => void
}): Promise<{ readonly ticket: WebRecordingHandoffTicket; revoke(): Promise<void>; close(): void }> {
  if (!Number.isSafeInteger(options.lifetimeMs) || options.lifetimeMs <= 0 || options.lifetimeMs > 300000)
    throw proxyError('argument')
  const { crypto, channel, graph, execute } = options
  const now = options.now ?? (() => performance.now())
  const deadline = now() + options.lifetimeMs
  const ticket: WebRecordingHandoffTicket = Object.freeze({
    version: WEB_PROXY_VERSION,
    secret: randomHex(crypto, 32),
    rendezvous: randomHex(crypto, 32),
    incarnation: randomHex(crypto, 32),
    lifetimeMs: options.lifetimeMs,
    expiresAtMonotonic: deadline
  })
  const ownerChallenge = randomHex(crypto, 32)
  let candidate = ''
  let accepted = false
  let confirmed = false
  let closed = false
  let endpoint: Awaited<ReturnType<typeof createWebRecordingProxy>> | undefined
  let serial = Promise.resolve()
  let queued = 0
  const unsubscribe = channel.subscribe(raw => {
    if (closed) return
    const packet = parse(raw)
    if (!packet || !matches(packet, ticket)) return
    if (queued >= 32) return
    queued++
    serial = serial
      .then(async () => {
        if (closed || (!confirmed && now() >= deadline)) return
        const p = parse(raw)
        if (!p || !matches(p, ticket) || !validRandom(p.c)) return
        if (p.t === 'init' && p.o === '' && !accepted && (!candidate || candidate === p.c)) {
          if (
            !(await verifyMac(crypto, ticket.secret, fields(ticket, p.t, p.c, p.o), p.m)) ||
            closed ||
            now() >= deadline
          )
            return
          candidate = p.c
          await send(crypto, channel, ticket, 'challenge', candidate, ownerChallenge)
        } else if (p.t === 'proof' && candidate === p.c && p.o === ownerChallenge) {
          if (
            !(await verifyMac(crypto, ticket.secret, fields(ticket, p.t, p.c, p.o), p.m)) ||
            closed ||
            now() >= deadline
          )
            return
          // Consume before asynchronous setup: at most one endpoint may win.
          if (!accepted) {
            accepted = true
            endpoint = await createWebRecordingProxy({
              crypto,
              channel,
              secret: ticket.secret,
              rendezvous: ticket.rendezvous,
              ownerChallenge,
              clientChallenge: candidate,
              role: 'owner',
              execute: payload => execute(graph, payload),
              onRelease: options.onRelease,
              onClose: options.onClose
            })
          }
          if (closed) {
            endpoint?.close()
            return
          }
          if (now() >= deadline) {
            expire()
            return
          }
          if (
            !(await send(
              crypto,
              channel,
              ticket,
              'accepted',
              candidate,
              ownerChallenge,
              () => !closed && now() < deadline
            ))
          )
            expire()
        } else if (p.t === 'confirm' && accepted && candidate === p.c && p.o === ownerChallenge) {
          if (
            !(await verifyMac(crypto, ticket.secret, fields(ticket, p.t, p.c, p.o), p.m)) ||
            closed ||
            (!confirmed && now() >= deadline)
          )
            return
          confirmed = true
          clearTimeout(expiryTimer)
          await send(crypto, channel, ticket, 'confirmed', candidate, ownerChallenge)
        }
      })
      .catch(() => {
        closed = true
        clearTimeout(expiryTimer)
        unsubscribe()
        channel.close()
        endpoint?.close()
      })
      .finally(() => {
        queued--
      })
  })
  const expire = (): void => {
    if (closed || confirmed) return
    const remaining = deadline - now()
    if (remaining > 0) {
      expiryTimer = setTimeout(expire, remaining)
      return
    }
    closed = true
    unsubscribe()
    if (endpoint) endpoint.close()
    else channel.close()
  }
  let expiryTimer = setTimeout(expire, options.lifetimeMs)
  const close = (): void => {
    if (closed) return
    closed = true
    clearTimeout(expiryTimer)
    unsubscribe()
    if (endpoint) endpoint.close()
    else channel.close()
  }
  return {
    ticket,
    close,
    async revoke() {
      try {
        if (endpoint) await endpoint.revoke()
      } finally {
        close()
      }
    }
  }
}
export async function importWebRecordingHandoff(options: {
  readonly crypto: WebProxyCrypto
  readonly channel: WebRecordingProxyChannel
  readonly ticket: WebRecordingHandoffTicket
  readonly timeoutMs: number
}): Promise<Awaited<ReturnType<typeof createWebRecordingProxy>>> {
  const { crypto, channel, ticket } = options
  if (
    ticket.version !== WEB_PROXY_VERSION ||
    !validRandom(ticket.secret) ||
    !validRandom(ticket.rendezvous) ||
    !validRandom(ticket.incarnation) ||
    !Number.isSafeInteger(ticket.lifetimeMs) ||
    ticket.lifetimeMs <= 0 ||
    ticket.lifetimeMs > 300000 ||
    !Number.isFinite(options.timeoutMs) ||
    options.timeoutMs <= 0
  )
    throw proxyError('argument')
  const clientChallenge = randomHex(crypto, 32)
  return new Promise((resolve, reject) => {
    let closed = false
    let ownerChallenge = ''
    let confirmed = false
    let confirmationWire = ''
    let confirmationTimer: ReturnType<typeof setTimeout> | undefined
    let serial = Promise.resolve()
    let queued = 0
    const fail = (code: string): void => {
      if (closed) return
      closed = true
      clearTimeout(timer)
      clearInterval(retryTimer)
      unsubscribe()
      channel.close()
      reject(proxyError(code))
    }
    const timer = setTimeout(() => fail('timeout'), options.timeoutMs)
    const retryTimer = setInterval(
      () => {
        if (confirmed) {
          clearInterval(retryTimer)
        } else if (confirmationWire) {
          try {
            channel.postMessage(confirmationWire)
          } catch {
            clearInterval(retryTimer)
            unsubscribe()
          }
        } else if (!closed) {
          send(crypto, channel, ticket, ownerChallenge ? 'proof' : 'init', clientChallenge, ownerChallenge).catch(() =>
            fail('protocol')
          )
        }
      },
      Math.max(10, Math.min(100, options.timeoutMs / 4))
    )
    const unsubscribe = channel.subscribe(raw => {
      if (closed && !confirmationWire) return
      const packet = parse(raw)
      if (!packet || !matches(packet, ticket) || packet.c !== clientChallenge) return
      if (queued >= 32) return
      queued++
      serial = serial
        .then(async () => {
          const p = parse(raw)
          if (!p || !matches(p, ticket) || p.c !== clientChallenge || !validRandom(p.o)) return
          if (
            (p.t !== 'challenge' && p.t !== 'accepted' && p.t !== 'confirmed') ||
            !(await verifyMac(crypto, ticket.secret, fields(ticket, p.t, p.c, p.o), p.m)) ||
            (closed && !confirmationWire)
          )
            return
          if (p.t === 'confirmed' && confirmationWire && ownerChallenge === p.o) {
            confirmed = true
            clearInterval(retryTimer)
            clearTimeout(confirmationTimer)
            unsubscribe()
          } else if (!closed && p.t === 'challenge' && (!ownerChallenge || ownerChallenge === p.o)) {
            ownerChallenge = p.o
            await send(crypto, channel, ticket, 'proof', clientChallenge, ownerChallenge)
          } else if (!closed && p.t === 'accepted' && ownerChallenge === p.o) {
            const endpoint = await createWebRecordingProxy({
              crypto,
              channel,
              secret: ticket.secret,
              rendezvous: ticket.rendezvous,
              ownerChallenge,
              clientChallenge,
              role: 'client'
            })
            if (closed) {
              endpoint.close()
              return
            }
            const signature = await mac(
              crypto,
              ticket.secret,
              fields(ticket, 'confirm', clientChallenge, ownerChallenge)
            )
            if (closed) {
              endpoint.close()
              return
            }
            confirmationWire = JSON.stringify({
              v: WEB_PROXY_VERSION,
              r: ticket.rendezvous,
              i: ticket.incarnation,
              t: 'confirm',
              c: clientChallenge,
              o: ownerChallenge,
              m: signature
            })
            try {
              channel.postMessage(confirmationWire)
            } catch (error) {
              endpoint.close()
              throw error
            }
            closed = true
            clearTimeout(timer)
            confirmationTimer = setTimeout(
              () => {
                clearInterval(retryTimer)
                unsubscribe()
              },
              Math.min(ticket.lifetimeMs, 300000)
            )
            const stopConfirmation = (): void => {
              clearInterval(retryTimer)
              clearTimeout(confirmationTimer)
              unsubscribe()
            }
            resolve({
              ...endpoint,
              close() {
                stopConfirmation()
                endpoint.close()
              },
              async release(timeoutMs) {
                await endpoint.release(timeoutMs)
                stopConfirmation()
              },
              async revoke() {
                try {
                  await endpoint.revoke()
                } finally {
                  stopConfirmation()
                }
              }
            })
          }
        })
        .catch(() => fail('protocol'))
        .finally(() => {
          queued--
        })
    })
    send(crypto, channel, ticket, 'init', clientChallenge, '').catch(() => fail('protocol'))
  })
}
