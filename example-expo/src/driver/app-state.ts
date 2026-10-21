// example-expo/src/driver/app-state.ts

import type { AppStateReading } from './shared.ts'

export function readAppState(status: string | null | undefined): AppStateReading {
  if (status === null || status === undefined) return { state: 'unknown', foreground: false }
  return { state: status, foreground: status === 'active' }
}
