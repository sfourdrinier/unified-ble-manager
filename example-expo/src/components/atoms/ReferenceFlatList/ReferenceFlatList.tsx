import React, { useState } from 'react'
import { FlatList, Platform, View, type FlatListProps } from 'react-native'

/**
 * RN TV 0.86 wraps VirtualizedList's ScrollView in a focus guide with no flex
 * style. A flex-only inner list collapses to one point. Bound its height to the
 * actual available viewport on TV; keep the same virtualized content everywhere.
 */
export function ReferenceFlatList<Item>(props: FlatListProps<Item>) {
  const [height, setHeight] = useState<number | null>(null)
  if (!Platform.isTV) return <FlatList {...props} />
  return (
    <View style={{ flex: 1 }} onLayout={event => setHeight(event.nativeEvent.layout.height)}>
      <FlatList {...props} style={[props.style, height === null ? null : { flex: 0, height }]} />
    </View>
  )
}
