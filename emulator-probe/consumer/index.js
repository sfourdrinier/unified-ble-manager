// Probe entry: Hermes on this profile does not provide TextDecoder, but the
// UBM native-protocol JS boundary (v2-codec) requires it to decode native
// records. Polyfill it before anything else. Without this, native-to-JS
// delivery fails closed with `ReferenceError: Property 'TextDecoder' doesn't
// exist` (observed REAL-EMULATOR; the failure is surfaced, never swallowed).
import { TextDecoder, TextEncoder } from 'text-encoding'

const g = globalThis
if (typeof g.TextDecoder === 'undefined') {
  g.TextDecoder = TextDecoder
}
if (typeof g.TextEncoder === 'undefined') {
  g.TextEncoder = TextEncoder
}

import { AppRegistry } from 'react-native'
import { App } from './App'
import { name as appName } from './app.json'

AppRegistry.registerComponent(appName, () => App)
