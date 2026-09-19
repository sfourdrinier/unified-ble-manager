// __tests__/AppleExpoRuntimeSwiftSelectors.test.js
//
// UnifiedBleExpoRuntime.mm calls Swift methods through the generated
// BlePlx-Swift.h header. Swift exports `func name(label: T, completion:)` as the
// Objective-C selector `nameWithLabel:completion:`; a call written with the
// Swift spelling compiles only in Swift and breaks the iOS app build (it did,
// for requestPermission). The native-protocol gate does not compile this file
// against the Swift header, so this guard pins every such call.

const fs = require('node:fs')
const path = require('node:path')

const root = path.join(__dirname, '..')
const runtime = fs.readFileSync(path.join(root, 'ios', 'UnifiedBleExpoRuntime.mm'), 'utf8')
const radio = fs.readFileSync(path.join(root, 'ios', 'Owned', 'OwnedCoreBluetoothProtocolRadio.swift'), 'utf8')

function objcSelector(name, firstLabel, rest) {
  const capitalized = firstLabel.charAt(0).toUpperCase() + firstLabel.slice(1)
  return [`${name}With${capitalized}:`, ...rest.map(label => `${label}:`)].join('')
}

describe('Apple Expo runtime → Swift radio selectors', () => {
  it('imports CoreBluetooth before the generated Swift header', () => {
    const core = runtime.indexOf('#import <CoreBluetooth/CoreBluetooth.h>')
    const swift = runtime.indexOf('#import "BlePlx-Swift.h"')
    expect(core).toBeGreaterThanOrEqual(0)
    expect(swift).toBeGreaterThan(core)
  })

  it('calls requestPermission with its Objective-C selector', () => {
    const match = radio.match(/@objc public func (requestPermission)\(\s*(\w+):[^,]+,\s*(\w+):/)
    expect(match).not.toBeNull()
    const [, name, first, second] = match
    const selector = objcSelector(name, first, [second])
    expect(selector).toBe('requestPermissionWithTimeoutMs:completion:')
    const [head, tail] = selector.split(':')
    expect(runtime).toMatch(new RegExp(`\\[radio ${head}:[\\s\\S]*?\\n\\s*${tail}:`))
    expect(runtime).not.toMatch(/\[radio requestPermission:/)
  })
})
