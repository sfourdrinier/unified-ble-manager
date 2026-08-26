/**
 * Retention bounds for the convenience scan-state projections (`useScanState`
 * and the manager's presence map).
 *
 * Safety bounds on a package-owned cache, not on delivery. They cap how many
 * peers a *derived view* remembers; they never drop an observation the consumer
 * asked for -- a consumer that wants every report iterates the scan session's
 * observation stream, whose budget is caller-supplied through `delivery`. Making
 * these tunable would let a dense environment grow unbounded retained state
 * inside a React render path, which is the failure they exist to prevent.
 */
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
