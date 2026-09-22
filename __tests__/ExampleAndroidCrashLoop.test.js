const fs = require('fs')
const path = require('path')

const root = path.join(__dirname, '..')

describe('Classic example Application startup (crash-loop regression)', () => {
  test('MainApplication builds reactHost lazily (applicationContext is null at construction)', () => {
    const source = fs.readFileSync(
      path.join(root, 'example/android/app/src/main/java/com/bleplxexample/MainApplication.kt'),
      'utf8'
    )
    // Eager `override val reactHost: ReactHost = getDefaultReactHost(applicationContext, …)`
    // NPEs at instantiation (mBase not yet attached) — instant crash-loop on
    // every launch, proven on an x86_64 API-34 emulator. Lazy defers first
    // access past onCreate, when the base context is attached.
    expect(source).toContain('override val reactHost: ReactHost by lazy')
    expect(source).not.toMatch(/override val reactHost:\s*ReactHost\s*=\s*\n?\s*getDefaultReactHost/)
  })
})
