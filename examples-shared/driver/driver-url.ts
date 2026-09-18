// examples-shared/driver/driver-url.ts
//
// Where a host finds the control server. Each host adapter picks its own
// source (Metro bundle URL, page query, CLI flag); the rules live here once.

/** Port of the control server (`examples-shared/driver/server/cli.mjs serve`). 8790 is used by other tooling. */
export const DRIVER_PORT = 8795
/** WebSocket path hosts connect to. The control CLI uses `/control`. */
export const DRIVER_HOST_PATH = '/host'
/** A desktop host runs on the same machine as the control server. */
export const LOCAL_DRIVER_URL = `ws://127.0.0.1:${DRIVER_PORT.toString()}${DRIVER_HOST_PATH}`

const SERVED_BUNDLE = /^https?:\/\/(\[[^\]]+\]|[^/:?#]+)(?::\d+)?\//
const WEBSOCKET_URL = /^wss?:\/\/[^/?#\s]+(\/[^\s]*)?$/

export type DriverUrlResolution = { readonly url: string; readonly reason: string } | { readonly url: null; readonly reason: string }

/**
 * A device reaches the control server on the same host that served its code
 * (Metro bundle or dev-server page). An embedded bundle (`file://`) has no
 * such host, so there is nothing to derive.
 */
export function driverUrlFromScriptUrl(scriptUrl: string | null, port: number = DRIVER_PORT): string | null {
  if (scriptUrl === null) return null
  const host = SERVED_BUNDLE.exec(scriptUrl)?.[1]
  if (host === undefined) return null
  return `ws://${host}:${port.toString()}${DRIVER_HOST_PATH}`
}

/**
 * An explicit URL from the host's configuration (`?driver=` query, env
 * variable, CLI flag). A malformed value is refused with its reason rather
 * than replaced by a default.
 */
export function explicitDriverUrl(value: string | null | undefined, source: string): DriverUrlResolution | null {
  if (value === null || value === undefined || value.length === 0) return null
  if (value === 'off') return { url: null, reason: `${source}=off` }
  if (!WEBSOCKET_URL.test(value)) return { url: null, reason: `${source} is not a ws:// or wss:// URL: ${value}` }
  return { url: value, reason: source }
}

/** The `driver` query parameter of a page URL, as used by the browser hosts. */
export function driverUrlFromQuery(search: string): DriverUrlResolution | null {
  return explicitDriverUrl(new URLSearchParams(search).get('driver'), '?driver')
}
