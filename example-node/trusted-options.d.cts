export interface TrustedDesktopOptions {
  readonly adapterId?: string
  readonly connectionPolicy?: { readonly mode: 'le-bearer'; readonly daemonUniqueOwner: string }
}
export function trustedDesktopOptions(
  backend: string,
  adapterId: string | undefined,
  daemonUniqueOwner: string | undefined
): TrustedDesktopOptions
