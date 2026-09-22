import React from 'react'
import { ScrollView } from 'react-native'
import { Container } from './ScreenDefaultContainer.styled'

export type ScreenDefaultContainerProps = {
  children: React.ReactNode
  // Finding 231: a screen of stacked actions is taller than a phone, and
  // without this its lower half is unreachable (the dashboard's restoration
  // claim could not be tapped on a Samsung SM-A376U1). Opt-in, because a
  // screen whose body is a FlatList scrolls itself and must not be nested in
  // another scroll view of the same orientation.
  scrollable?: boolean
}

export function ScreenDefaultContainer({ children, scrollable = false }: ScreenDefaultContainerProps) {
  return (
    <Container edges={['bottom', 'left', 'right']}>
      {scrollable ? (
        <ScrollView contentContainerStyle={{ flexGrow: 1 }} keyboardShouldPersistTaps="handled">
          {children}
        </ScrollView>
      ) : (
        children
      )}
    </Container>
  )
}
