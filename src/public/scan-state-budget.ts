export const MAX_PUBLIC_SCAN_STATE_ENTRIES = 256
export const MAX_PUBLIC_SCAN_STATE_BYTES = 256 * 1024

export function estimatePublicPeerRetentionBytes(peer: {
  readonly id: string
  readonly name: string | null
  readonly lastAdvertisement?: {
    readonly manufacturerData?: readonly { readonly data: Readonly<Uint8Array> }[] | null
    readonly serviceData?: readonly { readonly data: Readonly<Uint8Array> }[] | null
  } | null
}): number {
  let bytes = 64 + peer.id.length * 2 + (peer.name?.length ?? 0) * 2
  for (const entry of peer.lastAdvertisement?.manufacturerData ?? []) bytes += entry.data.byteLength
  for (const entry of peer.lastAdvertisement?.serviceData ?? []) bytes += entry.data.byteLength
  return bytes
}
