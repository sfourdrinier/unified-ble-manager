import { contractError } from '../backend-contract/errors'
import type { ScanPlatformOptions } from '../public/ble-manager'
import { assertPublicScanOptions } from '../public/ble-manager'
import { decodeWinRtScanPlatformOptions } from '../backend-contract/advertisement'

/** Fail-closed decoder for the same platform options accepted by public scan(). */
export function decodeIpcScanPlatform(value: unknown): ScanPlatformOptions | undefined {
  if (value === undefined) return undefined
  if (typeof value !== 'object' || value === null || Array.isArray(value))
    throw contractError('argument.invalid', 'scan', 'ipc.scan.platform')
  const input = Object.fromEntries(Object.entries(value))
  const kind = input.kind
  if (kind === 'winrt') {
    return decodeWinRtScanPlatformOptions(input, 'ipc.scan.platform-values')
  }
  if (kind === 'corebluetooth' || kind === 'web' || kind === 'electron' || kind === 'tauri') {
    if (Object.keys(input).length !== 1) throw contractError('argument.invalid', 'scan', 'ipc.scan.platform-fields')
    return { kind }
  }
  if (
    kind !== 'android' ||
    Object.keys(input).some(key => !['kind', 'mode', 'callbackType', 'reportDelayMs', 'legacy', 'phy'].includes(key))
  )
    throw contractError('argument.invalid', 'scan', 'ipc.scan.platform-fields')
  const mode = input.mode,
    callbackType = input.callbackType,
    reportDelayMs = input.reportDelayMs,
    legacy = input.legacy,
    phy = input.phy
  if (
    (mode !== undefined &&
      mode !== 'low-power' &&
      mode !== 'balanced' &&
      mode !== 'low-latency' &&
      mode !== 'opportunistic') ||
    (callbackType !== undefined &&
      callbackType !== 'all-matches' &&
      callbackType !== 'first-match' &&
      callbackType !== 'match-lost') ||
    (reportDelayMs !== undefined && typeof reportDelayMs !== 'number') ||
    (legacy !== undefined && typeof legacy !== 'boolean') ||
    (phy !== undefined && phy !== 'all-supported' && phy !== '1m' && phy !== 'coded')
  )
    throw contractError('argument.invalid', 'scan', 'ipc.scan.platform-values')
  const platform = {
    kind,
    ...(mode === undefined ? {} : { mode }),
    ...(callbackType === undefined ? {} : { callbackType }),
    ...(reportDelayMs === undefined ? {} : { reportDelayMs }),
    ...(legacy === undefined ? {} : { legacy }),
    ...(phy === undefined ? {} : { phy })
  }
  const checked: ScanPlatformOptions = platform
  assertPublicScanOptions({ platform: checked })
  return checked
}
