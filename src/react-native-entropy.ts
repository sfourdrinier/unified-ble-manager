import { contractError } from './backend-contract/errors'

export const REACT_NATIVE_IDENTITY_ENTROPY_BYTES = 40

export async function createNativeRandomBytesSource(
  getRandomBytes: (length: number) => Promise<readonly number[]>
): Promise<(length: number) => Uint8Array> {
  const values = await getRandomBytes(REACT_NATIVE_IDENTITY_ENTROPY_BYTES)
  const pool = bytesFromIntegerList(values)
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

function bytesFromIntegerList(values: readonly number[]): Uint8Array {
  if (!Array.isArray(values)) {
    throw contractError('protocol.malformed', 'core', 'host-identity.native-random-bytes')
  }
  const out = new Uint8Array(values.length)
  for (let index = 0; index < values.length; index += 1) {
    const value = values[index]
    if (!Number.isInteger(value) || value < 0 || value > 255) {
      throw contractError('protocol.malformed', 'core', 'host-identity.native-random-bytes.value')
    }
    out[index] = value
  }
  return out
}
