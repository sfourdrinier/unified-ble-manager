// example-expo/src/driver/expo-driver-platform.ts
//
// Driver platform label for the Expo host adapter. react-native-tvos keeps
// Platform.OS === 'ios' on Apple TV and signals TV through Platform.isTV, so
// the adapter maps that combination to `tvos`: the control server, the event
// label `<host>/<platform>` and the host id stay distinct from the iPhone.

export function resolveExpoDriverPlatform(os: string, isTV: boolean): string {
  if (os === 'ios' && isTV) return 'tvos'
  return os
}
