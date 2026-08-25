import { contractError } from './backend-contract/errors'

export const REACT_NATIVE_IDENTITY_ENTROPY_BYTES = 40

export async function createNativeRandomBytesSource(
  getRandomBytes: (length: number) => Promise<string>
): Promise<(length: number) => Uint8Array> {
  const encoded = await getRandomBytes(REACT_NATIVE_IDENTITY_ENTROPY_BYTES)
  const pool = decodeBase64Entropy(encoded)
  if (pool.byteLength !== REACT_NATIVE_IDENTITY_ENTROPY_BYTES) {
    throw contractError('protocol.malformed', 'core', 'host-identity.native-random-bytes.length')
  }
  let offset = 0
  return (length: number): Uint8Array => {
    if (!Number.isSafeInteger(length) || length <= 0 || offset + length > pool.byteLength) {
      throw contractError('capability.unsupported', 'core', 'host-identity.native-random-bytes.exhausted')
    }
    const slice = pool.subarray(offset, offset + length)
    offset += length
    return new Uint8Array(slice)
  }
}

function decodeBase64Entropy(encoded: string): Uint8Array {
  if (typeof encoded !== 'string' || encoded.length === 0) {
    throw contractError('protocol.malformed', 'core', 'host-identity.native-random-bytes')
  }
  const nodeDecoded = nodeDecodeBase64(encoded)
  if (nodeDecoded !== null) return nodeDecoded
  if (typeof globalThis.atob !== 'function') {
    throw contractError('capability.unsupported', 'core', 'host-identity.secure-randomness')
  }
  const binary = globalThis.atob(encoded)
  const out = new Uint8Array(binary.length)
  for (let index = 0; index < binary.length; index += 1) {
    out[index] = binary.charCodeAt(index)
  }
  return out
}

function nodeDecodeBase64(encoded: string): Uint8Array | null {
  if (!('Buffer' in globalThis)) return null
  const buffer = Reflect.get(globalThis, 'Buffer')
  if (typeof buffer !== 'function') return null
  const from = Reflect.get(buffer, 'from')
  if (typeof from !== 'function') return null
  const decoded = Reflect.apply(from, buffer, [encoded, 'base64'])
  if (!(decoded instanceof Uint8Array)) return null
  return new Uint8Array(decoded)
}
