// examples-shared/driver/browser/host-facts.ts
//
// Host facts every browser-engine host (Web, Tauri webview, Electron renderer)
// reads the same way: the WebSocket, the OS from the user agent, and page
// visibility as the app-state source.

import type { AppStateReading, AppStateSource } from '../host.ts'
import type { DriverSocket, DriverSocketHandlers } from '../remote-channel.ts'

export function browserSocket(url: string, handlers: DriverSocketHandlers): DriverSocket {
  const socket = new WebSocket(url)
  socket.addEventListener('open', () => handlers.onOpen())
  socket.addEventListener('message', event => handlers.onMessage(event.data))
  // A browser WebSocket error event carries no detail by design; the close that follows has the code.
  socket.addEventListener('error', () => handlers.onError(`WebSocket error on ${url}`))
  socket.addEventListener('close', event => handlers.onClose(event.code, event.reason))
  return { send: text => socket.send(text), close: () => socket.close() }
}

const USER_AGENT_PLATFORMS: readonly (readonly [RegExp, string])[] = [
  [/Android/i, 'android'],
  [/iPhone|iPad|iPod/i, 'ios'],
  [/Mac OS X|Macintosh/i, 'macos'],
  [/Windows/i, 'windows'],
  [/Linux|X11|CrOS/i, 'linux']
]

/** The OS named by the user agent; `unknown` when it names none this driver knows. */
export function platformFromUserAgent(userAgent: string): string {
  return USER_AGENT_PLATFORMS.find(([pattern]) => pattern.test(userAgent))?.[1] ?? 'unknown'
}

const ENGINES: readonly (readonly [RegExp, string])[] = [
  [/Electron\/([\d.]+)/, 'Electron'],
  [/Chrome\/([\d.]+)/, 'Chrome'],
  [/Firefox\/([\d.]+)/, 'Firefox'],
  [/Version\/([\d.]+).*Safari/, 'Safari']
]

/** The engine and version the user agent names. A page cannot read the OS version, so hosts report this instead. */
export function engineFromUserAgent(userAgent: string): string {
  for (const [pattern, name] of ENGINES) {
    const version = pattern.exec(userAgent)?.[1]
    if (version !== undefined) return `${name} ${version}`
  }
  return 'unknown-engine'
}

function readVisibility(document: Document): AppStateReading {
  return { state: document.visibilityState, foreground: document.visibilityState === 'visible' }
}

/** Page visibility is the app state of a browser-engine host: `visible` is foreground, `hidden` is not. */
export function documentAppState(document: Document): AppStateSource {
  return {
    current: () => readVisibility(document),
    subscribe(listener) {
      const onChange = () => listener(readVisibility(document))
      document.addEventListener('visibilitychange', onChange)
      return () => document.removeEventListener('visibilitychange', onChange)
    }
  }
}
