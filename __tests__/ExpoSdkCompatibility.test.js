// __tests__/ExpoSdkCompatibility.test.js

const semver = require('semver')
const manifest = require('../package.json')

describe('Expo consumer peer compatibility', () => {
  test.each(['57.0.0', '57.0.30', '58.0.0', '58.0.16'])(
    'admits stable Expo %s without changing the React Native floor',
    version => {
      expect(semver.satisfies(version, manifest.peerDependencies.expo)).toBe(true)
      expect(manifest.peerDependencies['react-native']).toBe('>=0.86.0')
      expect(manifest.peerDependenciesMeta.expo).toEqual({ optional: true })
    }
  )

  test.each(['56.0.23', '58.0.0-beta.1', '59.0.0'])(
    'does not advertise unqualified Expo %s',
    version => {
      expect(semver.satisfies(version, manifest.peerDependencies.expo)).toBe(false)
    }
  )
})
