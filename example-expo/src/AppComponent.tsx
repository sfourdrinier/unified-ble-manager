import React from 'react'
import { SafeAreaProvider } from 'react-native-safe-area-context'
import { ThemeProvider } from 'styled-components'
import Toast from 'react-native-toast-message'
import { commonTheme } from './theme/theme'
import { Navigation } from './navigation'
import { useRemoteDriverLifecycle } from './driver/use-driver'

export function AppComponent() {
  useRemoteDriverLifecycle()
  return (
    <SafeAreaProvider>
      <ThemeProvider theme={commonTheme}>
        <Navigation />
        <Toast />
      </ThemeProvider>
    </SafeAreaProvider>
  )
}
