import type { PressableProps } from 'react-native'

// A Pressable works with both touch and TV remote activation. Native Switch
// has no tvOS Fabric implementation; accessibilityRole does not instantiate it.
export function streamToggleProps(label: string, checked: boolean, onChange: (value: boolean) => void) {
  return {
    accessibilityRole: 'switch',
    accessibilityLabel: `${label} on next start`,
    accessibilityState: { checked },
    focusable: true,
    onPress: () => onChange(!checked)
  } satisfies PressableProps
}
