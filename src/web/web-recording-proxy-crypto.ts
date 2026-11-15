/** Private protocol primitives. No browser global is read at module import. */
export type WebProxyCrypto = typeof globalThis.crypto
export const WEB_PROXY_VERSION = 'ubm-web-recording-proxy/2'
export const WEB_PROXY_LIMIT = 65536
const encoder = new TextEncoder()
const decoder = new TextDecoder('utf-8', { fatal: true })
export const encode = (s: string): Uint8Array<ArrayBuffer> => encoder.encode(s)
export const decode = (b: ArrayBuffer): string => decoder.decode(b)
export const hex = (bytes: Uint8Array): string => Array.from(bytes, b => b.toString(16).padStart(2, '0')).join('')
export function unhex(value: string, length: number): Uint8Array<ArrayBuffer> {
  if (value.length !== length * 2 || !/^[0-9a-f]+$/.test(value)) throw new Error('invalid protocol bytes')
  const result = new Uint8Array(length)
  for (let i = 0; i < length; i++) result[i] = parseInt(value.slice(i * 2, i * 2 + 2), 16)
  return result
}
export function randomHex(crypto: WebProxyCrypto, size: number): string {
  return hex(crypto.getRandomValues(new Uint8Array(size)))
}
export async function mac(crypto: WebProxyCrypto, secret: string, fields: readonly string[]): Promise<string> {
  const key = await crypto.subtle.importKey('raw', unhex(secret, 32), { name: 'HMAC', hash: 'SHA-256' }, false, [
    'sign'
  ])
  return hex(new Uint8Array(await crypto.subtle.sign('HMAC', key, encode(JSON.stringify(fields)))))
}
export async function verifyMac(
  crypto: WebProxyCrypto,
  secret: string,
  fields: readonly string[],
  signature: string
): Promise<boolean> {
  try {
    const key = await crypto.subtle.importKey('raw', unhex(secret, 32), { name: 'HMAC', hash: 'SHA-256' }, false, [
      'verify'
    ])
    return await crypto.subtle.verify('HMAC', key, unhex(signature, 32), encode(JSON.stringify(fields)))
  } catch {
    return false
  }
}
export async function directionalKey(
  crypto: WebProxyCrypto,
  secret: string,
  ownerChallenge: string,
  clientChallenge: string,
  direction: string
): Promise<Awaited<ReturnType<WebProxyCrypto['subtle']['importKey']>>> {
  const key = await crypto.subtle.importKey('raw', unhex(secret, 32), 'HKDF', false, ['deriveKey'])
  return crypto.subtle.deriveKey(
    {
      name: 'HKDF',
      hash: 'SHA-256',
      salt: encode(JSON.stringify([WEB_PROXY_VERSION, ownerChallenge, clientChallenge])),
      info: encode(direction)
    },
    key,
    { name: 'AES-GCM', length: 256 },
    false,
    ['encrypt', 'decrypt']
  )
}
export function nonce(sequence: number): Uint8Array<ArrayBuffer> {
  if (!Number.isSafeInteger(sequence) || sequence < 1) throw new Error('sequence exhausted')
  const result = new Uint8Array(12)
  new DataView(result.buffer).setBigUint64(4, BigInt(sequence))
  return result
}
export function proxyError(code: string): Error & { code: string } {
  return Object.assign(new Error(`web recording proxy: ${code}`), { code })
}
