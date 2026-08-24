// __tests__/Docs.consumer.test.js

/**
 * Consumer-facing markdown must describe the current published package.
 * Maintainer/archive pages can keep older labels; teaching pages cannot.
 */
const fs = require('fs')
const path = require('path')

const root = path.join(__dirname, '..')
const read = relativePath => fs.readFileSync(path.join(root, relativePath), 'utf8').replace(/\r\n/g, '\n')
const packageVersion = JSON.parse(read('package.json')).version
const stable40 = /^4\.0\.\d+$/u.test(packageVersion)
const rcVersionMatch = /^4\.0\.0-rc\.\d+(?:\.\d+)?$/u.exec(packageVersion)
const alphaVersionMatch = /^4\.0\.0-alpha\.(\d+)$/u.exec(packageVersion)
if (!stable40 && rcVersionMatch === null && alphaVersionMatch === null) {
  throw new Error(`Expected a 4.0.x stable, a 4.0 RC, or a 4.0 alpha package version, received ${packageVersion}`)
}
const currentAlpha = alphaVersionMatch === null ? null : Number(alphaVersionMatch[1])
const previousAlphaVersion = currentAlpha === null ? null : `v4.0.0-alpha.${String(currentAlpha - 1)}`

const architectureAuthorityDocuments = [
  'README.md',
  'ROADMAP.4.0.md',
  'MIGRATION_4.0.md',
  'RELEASE.md',
  'docs/GAPS.4.0.md',
  'docs/GETTING_STARTED.md',
  'docs/PLATFORMS.md',
  'docs/ELECTRON.md',
  'docs/NODE.md',
  'docs/EXPO_PLUGIN.md',
  'docs/BACKGROUND.md',
  'docs/WEB.md',
  'docs/TVOS.md',
  'docs/PERFORMANCE.md',
  'docs/BONDING.md'
]

const transitionalCharacterizationDocuments = ['docs/DISCOVERY_AND_PROFILES.md']

const currentPublicGuideDocuments = ['docs/CONNECTION_MANAGER.md', 'docs/HELPERS.md']

const deterministicExampleDocuments = ['example-electron/README.md']
const liveExampleDocuments = ['example-web/README.md']

const supersededAuthorityDocuments = [
  'ROADMAP.md',
  'docs/FIX_TRACKER.4.0.md',
  'docs/FIX_TRACKER.4.0-round2.md',
  'docs/FIX_TRACKER.4.0-round3.md'
]

const canonicalAdrDocuments = [
  'docs/ADR/2026-07-4.0-public-api.md',
  'docs/ADR/2026-07-4.0-backend-contract.md',
  'docs/ADR/2026-07-4.0-capability-registry.md',
  'docs/ADR/2026-07-4.0-boundary.md',
  'docs/ADR/2026-07-4.0-rn-restoration-bootstrap.md',
  'docs/ADR/2026-07-4.0-packaging.md',
  'docs/ADR/2026-07-4.0-open-source-governance.md',
  'docs/ADR/2026-08-4.0-public-contract-reset.md'
]

const deletedTransitionalAdrs = [
  'docs/ADR/2026-07-4.0-host-and-bytes.md',
  'docs/ADR/2026-07-4.0-electron-macos-corebluetooth.md',
  'docs/ADR/2026-07-4.0-owned-core-and-electron-natives.md'
]

describe('consumer documentation matches the published package', () => {
  test('current public documentation follows the package release channel', () => {
    if (stable40 || rcVersionMatch) {
      expect(packageVersion).toMatch(/^4\.0\.\d+(?:-rc\.\d+(?:\.\d+)?)?$/u)
      return
    }
    for (const document of architectureAuthorityDocuments) {
      const withoutDeclaredPreviousRelease = read(document).replaceAll(previousAlphaVersion, '')
      const alphaReferences = [...withoutDeclaredPreviousRelease.matchAll(/alpha\.(\d+)/gu)]
      for (const reference of alphaReferences) {
        expect(Number(reference[1])).toBe(currentAlpha)
      }
    }
  })

  test('controlling implementation plan records the clean-baseline decisions', () => {
    const plan = read('docs/UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md')

    expect(plan).toContain('new open-source package with no production users')
    expect(plan).toContain('one versioned backend contract')
    expect(plan).toContain('one shared manager/policy core')
    expect(plan).toContain('All payloads are `Uint8Array`')
    expect(plan).toContain('AbortSignal')
    expect(plan).toContain('No static host capability matrix')
    expect(plan).toContain('No duplicated Base64 and byte method families')
    expect(plan).toContain('does not wrap Noble')
    expect(plan).toContain('Meta Quest is deferred to 4.1')
    expect(plan).toContain('It is not a 4.0 work package, evidence requirement, or release blocker')
    expect(plan).toContain('explicitly deferred to 4.1')
  })

  test('platform support truth is generated from evidence rather than a maintained host matrix', () => {
    const platforms = read('docs/PLATFORMS.md')
    expect(platforms).toContain('generated/PLATFORM_SUPPORT.md')
    expect(platforms).not.toContain('| Host/backend |')
  })

  test('gap inventory separates implemented code from missing physical proof', () => {
    const gaps = read('docs/GAPS.4.0.md')

    expect(gaps).toContain('Current implementation and evidence inventory')
    expect(gaps).toContain('Implementation/package state')
    expect(gaps).toContain('Remaining evidence work')
    expect(gaps).toContain('Implemented contract/core/TCK path')
    expect(gaps).toContain('not architecture authority')
    expect(gaps).toContain('implementation proof')
    expect(gaps).not.toContain('WinRT remains incomplete')
    expect(gaps).not.toContain('The pre-4.0 source tree contains a transitional')
  })

  test('plan keeps Quest out of the 4.0 execution graph and Android acceptance checklist', () => {
    const plan = read('docs/UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md')
    const graphStart = plan.indexOf('### 24.1 Hard dependency graph')
    const graphEnd = plan.indexOf('### 24.2 Recommended integration order', graphStart)
    const graph = plan.slice(graphStart, graphEnd)

    expect(graphStart).toBeGreaterThanOrEqual(0)
    expect(graphEnd).toBeGreaterThan(graphStart)
    expect(graph).not.toMatch(/\bQUEST\b/)
    expect(graph.match(/\bQuest\b/g)).toHaveLength(1)
    expect(graph).toContain('deferred 4.1 Quest work depends on frozen shared surfaces')
    expect(plan).not.toContain('and Quest build/runtime proof where claimed.')
  })

  test('plan and canonical ADRs retain normative capability, ownership, event, and transport contracts', () => {
    const plan = read('docs/UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md')
    const publicApi = read('docs/ADR/2026-07-4.0-public-api.md')
    const backendContract = read('docs/ADR/2026-07-4.0-backend-contract.md')

    expect(plan).toContain("'supported' | 'limited' | 'unsupported' | 'unavailable'")
    expect(plan).toContain('selectedSchemaRange')
    expect(plan).toContain('implementationOrigin')
    expect(plan).toContain('CapabilityEvidence')
    expect(plan).toContain('CapabilityTckBinding')
    expect(plan).toContain('Unsupported operations are registered as `unsupported`')
    expect(plan).toContain('highest common version')
    expect(plan).toContain('complete selected `NegotiatedVersionTuple` becomes immutable attachment data')
    expect(plan).toContain('Malformed offers, duplicate handshakes')
    expect(plan).toContain('BoundedAsyncStream<BackendEvent>')
    expect(plan).toContain('Every observation field is `present`, `absent`, or `unavailable`')
    expect(plan).toContain('advertisementPayload')
    expect(plan).toContain('scanResponsePayload')
    expect(plan).toContain('owner lease, connection generation, database')
    expect(plan).toContain('C++ JSI-owned binary payload transport')
    expect(plan).toContain('Codegen/TurboModule control and bootstrap methods carry metadata only')
    expect(plan).toContain('prove that none carries BLE bytes')
    expect(plan).not.toMatch(/\bscanAlreadyActive\b/)
    expect(plan).not.toMatch(/\bmanagerDestroyed\b/)
    expect(plan).not.toContain('TurboModule-supported `ArrayBuffer`/typed binary values')
    expect(plan).not.toContain('TurboModule binary round-trip')
    expect(plan).not.toContain('Unsupported operations are absent capabilities')

    expect(publicApi).toContain('only when no registered borrowers')
    expect(publicApi).toContain('settled revocation')
    expect(publicApi).toContain('atomic, verified ownership')
    expect(backendContract).toContain('BoundedAsyncStream<BackendEvent>')
    expect(backendContract).toContain('owner lease')
  })

  test.each(architectureAuthorityDocuments)('%s identifies the controlling architecture authority', relativePath => {
    const document = read(relativePath)

    expect(document.split('\n')[0]).toBe(`<!-- ${relativePath} -->`)
    expect(document).toContain('UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md')
  })

  test.each(transitionalCharacterizationDocuments)(
    '%s labels inherited behavior as transitional characterization',
    relativePath => {
      const document = read(relativePath)

      expect(document.split('\n')[0]).toBe(`<!-- ${relativePath} -->`)
      expect(document).toMatch(/transitional source characterization|transitional source behavior|legacy manager/i)
      expect(document).toContain('UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md')
    }
  )

  test.each(currentPublicGuideDocuments)('%s describes the current clean-baseline package surface', relativePath => {
    const document = read(relativePath)

    expect(document.split('\n')[0]).toBe(`<!-- ${relativePath} -->`)
    expect(document).not.toMatch(/transitional source characterization|transitional source behavior|legacy manager/i)
    expect(document).toMatch(/BleManager|withConnection|scanUntil/)
  })

  test.each(deterministicExampleDocuments)('%s makes only deterministic package-surface claims', relativePath => {
    const document = read(relativePath)

    expect(document.split('\n')[0]).toBe(`<!-- ${relativePath} -->`)
    expect(document).toMatch(/without\s+claiming live Electron-radio support/)
    expect(document).toContain('not a substitute for device-lab validation')
    expect(document).not.toMatch(/legacy manager|transitional source/i)
  })

  test.each(liveExampleDocuments)(
    '%s describes a clean-baseline live harness without claiming retained evidence',
    relativePath => {
      const document = read(relativePath)

      expect(document.split('\n')[0]).toBe(`<!-- ${relativePath} -->`)
      expect(document).toContain('4.0 clean-baseline Web Bluetooth example')
      expect(document).toMatch(/does not itself create a\s+release evidence receipt/u)
      expect(document).not.toMatch(/legacy manager|transitional source/i)
    }
  )

  test.each(supersededAuthorityDocuments)('%s cannot compete with the clean-baseline authority', relativePath => {
    const document = read(relativePath)

    expect(document.split('\n')[0]).toBe(`<!-- ${relativePath} -->`)
    expect(document).toMatch(/historical|superseded|authority boundary/i)
    expect(document).toContain('UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md')
  })

  test('the eight canonical ADRs replace every transitional ADR path', () => {
    const adrDirectory = path.join(root, 'docs/ADR')
    const actual = fs.readdirSync(adrDirectory).sort()
    const expected = canonicalAdrDocuments.map(relativePath => path.basename(relativePath)).sort()

    expect(actual).toEqual(expected)
    deletedTransitionalAdrs.forEach(relativePath => {
      expect(fs.existsSync(path.join(root, relativePath))).toBe(false)
    })
  })

  test.each(canonicalAdrDocuments)('%s is an accepted canonical decision record', relativePath => {
    const document = read(relativePath)

    expect(document.split('\n')[0]).toBe(`<!-- ${relativePath} -->`)
    expect(document).toContain('**Status:** Accepted design baseline')
    expect(document).toContain('## Decision')
    expect(document).toContain('## Consequences and gates')
    expect(document).toContain('## Rejected alternatives')
  })

  test('canonical ADRs record the accepted binary, restoration, and scope decisions', () => {
    const boundary = read('docs/ADR/2026-07-4.0-boundary.md')
    const restoration = read('docs/ADR/2026-07-4.0-rn-restoration-bootstrap.md')
    const governance = read('docs/ADR/2026-07-4.0-open-source-governance.md')

    expect(boundary).toMatch(/one\s+owned, versioned C\+\+ JSI binary transport/)
    expect(boundary).toMatch(/never use Base64, a parallel bridge, a compatibility\s+fallback/)
    expect(boundary).toContain('Electron main is the only production radio owner')
    expect(restoration).toContain('exactly one `CBCentralManager`')
    expect(restoration).toContain('It never creates a second central')
    expect(governance).toContain('Meta Quest is explicitly deferred to 4.1')
    expect(governance).toContain('absent from 4.0 work packages')
  })

  test('roadmap rejects compatibility, dual APIs, static matrices, Noble, and reduced scope', () => {
    const roadmap = read('ROADMAP.4.0.md')

    expect(roadmap).toMatch(/not a compatibility release/)
    expect(roadmap).toContain('bytes-only public/backend BLE contracts')
    expect(roadmap).toContain('does not preserve a permanent 3.x API')
    expect(roadmap).toContain('Meta Quest, peripheral mode, Bluetooth Classic, LE Audio, L2CAP CoC')
    expect(roadmap).toContain('remain deferred to 4.1')
    expect(roadmap).toContain('Stable `4.0.0` defines the public 4.x package/API contract')
    expect(roadmap).toContain('`v4.0.0-alpha.40` is retained as the final alpha')
    expect(roadmap).not.toMatch(/hard compatibility guarantee/i)
    expect(roadmap).not.toMatch(/Base64 still available unless 5\.0/i)
    expect(roadmap).not.toMatch(/thin install\/import shim/i)
  })

  test('migration documents 4.0 without inventing a compatibility path', () => {
    const migration = read('MIGRATION_4.0.md')
    const release = read('RELEASE.md')

    expect(migration).toContain(packageVersion)
    expect(migration).not.toContain('hostSessionScope')
    expect(migration).toContain('Uint8Array')
    expect(migration).toContain('AbortSignal')
    expect(migration).toContain('manager.destroy()')
    expect(migration).toContain('not a source-compatible rename')
    expect(migration).toContain('new BleManager')
    expect(migration).toContain('startDeviceScan')
    expect(migration).toContain('connectToDevice')
    expect(migration).toContain('monitorCharacteristicForDevice')
    expect(migration).toContain('cancelTransaction')
    expect(migration).not.toContain('encode/decode explicitly through `unified-ble-manager/codecs`')
    expect(migration).not.toMatch(/zero-change (JS )?API/i)
    expect(migration).not.toMatch(/optional bytes codemod/i)

    expect(release).toContain('Release branch: `main`')
    expect(release).toContain('Stable SemVer and platform support qualification are independent')
    expect(release).toContain('git tag -a v4.0.0')
    expect(release).not.toMatch(/publishes the \*\*4\.0 dual identity\*\*/i)
  })

  test('teaching pages describe the current stable package, not an RC install or 3.x constructor', () => {
    const teachingPages = [
      'README.md',
      'docs/GETTING_STARTED.md',
      'docs/TAURI.md',
      'docs/ELECTRON.md',
      'docs/NODE.md',
      'docs/WEB.md',
      'docs/EXPO_PLUGIN.md',
      'example/README.md',
      'example-expo/README.md',
      'example-tauri/README.md',
      'example-web/README.md',
      'example-electron/README.md'
    ]
    for (const document of teachingPages) {
      const text = read(document)
      expect(text).not.toMatch(/Current prerelease/i)
      expect(text).not.toMatch(/immutable published prerelease/i)
      expect(text).not.toMatch(/4\.0\.0-rc\.\* versions publish to npm `latest`/)
      expect(text).not.toMatch(/RC5 remains reserved/)
      expect(text).not.toMatch(/Do not recreate `v4\.0\.0`/)
      expect(text).not.toMatch(/const\s+\w+\s*=\s*new\s+BleManager\s*\(/)
      expect(text).not.toMatch(/writeCharacteristicWithResponseForDevice/)
      expect(text).not.toMatch(/transactionId/)
    }
    expect(read('README.md')).toContain(packageVersion)
    expect(read('README.md')).toContain('createTauriBleManager')
    expect(read('docs/TAURI.md')).toContain('createTauriBleManager')
    expect(read('example-tauri/README.md')).toContain('createTauriBleManager')
  })

  test('public README provides current construction and plugin guidance without frozen slogans', () => {
    const readme = read('README.md')
    const changelog = read('CHANGELOG.md')
    const history = read('CHANGELOG_HISTORY.md')

    expect(readme).toContain(packageVersion)
    expect(readme).not.toContain('4.0.0 is the first stable release')
    expect(readme).toContain('pnpm add unified-ble-manager')
    expect(readme).not.toContain(`pnpm add unified-ble-manager@${packageVersion}`)
    expect(changelog).toContain('## [4.0.0-rc.0]')
    expect(history).toContain('## [4.0.0-alpha.40]')
    expect(readme).toContain('createReactNativeBleManager')
    expect(readme).not.toContain('hostSessionScope')
    expect(readme).toContain('Uint8Array')
    expect(readme).toContain('AbortSignal')
    expect(readme).not.toContain('iosNativeProtocolRestoration')
    expect(readme).toContain('timeoutMs')
    expect(readme).not.toMatch(
      /iosEnableRestoration|iosRestorationIdentifier|iosNativeProtocolRestorationIdentifier|androidEnableForegroundService/
    )
    expect(readme).not.toMatch(/new\s+BleManager\s*\(/)
  })

  test('RC4 candidate documentation preserves release, evidence, and deferral boundaries', () => {
    const readme = read('README.md')
    const release = read('RELEASE.md')
    const platforms = read('docs/PLATFORMS.md')

    expect(readme).toContain(packageVersion)
    expect(readme).toContain('npm trusted publishing/OIDC with provenance')
    expect(release).toContain('git tag -a "v$release_candidate"')
    expect(release).toContain('release_candidate=4.0.0-rc.N')
    expect(release).toContain('git tag -a v4.0.0')
    expect(release).toContain('npm trusted publishing/OIDC')
    expect(release).toContain('publishes with provenance')
    expect(platforms).toContain(
      `\`unified-ble-manager@${packageVersion}\` is the published **stable package/API** for the 4.x contract; it is immutable. Backend support labels remain evidence-derived and independent of this SemVer`
    )
    expect(platforms).toContain(
      'WinRT compilation or ABI loading, for example, is not by itself a Windows live-radio claim'
    )
    expect(platforms).toContain(
      'Meta Quest and the controllable nRF52840 fault-injection controller remain deferred to 4.1'
    )
  })

  test('platform pages make instantiated backend evidence, not static source behavior, authoritative', () => {
    const platforms = read('docs/PLATFORMS.md')
    const gaps = read('docs/GAPS.4.0.md')

    expect(platforms).toContain('not a static compatibility matrix')
    expect(platforms).toContain('typed capabilities of its instantiated backend')
    expect(gaps).toContain('not architecture authority')
    expect(gaps).toContain('implementation proof')
    expect(gaps).toContain('do not become physical-radio support evidence')
    expect(gaps).toContain('must never be presented as live-radio proof')
  })

  test('profile discovery documentation names only supported hyphenated package exports', () => {
    const discovery = read('docs/DISCOVERY_AND_PROFILES.md')
    const commands = read('docs/PROFILES_AND_COMMANDS.md')
    const supportedProfileSubpaths = [
      'unified-ble-manager/profiles/heart-rate',
      'unified-ble-manager/profiles/battery-service',
      'unified-ble-manager/profiles/device-information',
      'unified-ble-manager/profiles/health-thermometer',
      'unified-ble-manager/profiles/blood-pressure'
    ]

    for (const subpath of supportedProfileSubpaths) {
      expect(discovery).toContain(subpath)
      expect(commands).toContain(subpath)
    }
    expect(discovery).not.toMatch(
      /profiles\/(heartRate|battery(?!-service)|deviceInformation|healthThermometer|bloodPressure)/
    )
  })

  test('advanced-only profile helpers are imported from the advanced entrypoint', () => {
    const commands = read('docs/PROFILES_AND_COMMANDS.md')

    expect(commands).toContain(
      "import { defaultScanDelivery, firstNotification } from 'unified-ble-manager/advanced'"
    )
    expect(commands).not.toContain(
      "import { defaultScanDelivery, firstNotification } from 'unified-ble-manager'"
    )
  })

  test('README is a human teaching front door for the current package', () => {
    const readme = read('README.md')
    const teachingLead = readme.split('\n').slice(0, 40).join('\n')

    expect(readme).toMatch(/Sponsored by \[Imagi Explain\]\(https:\/\/imagiexplain\.com\)/)
    expect(readme).toContain('react-native-ble-plx')
    expect(readme).toMatch(/cross-platform|unified/)
    expect(readme).toContain('find')
    expect(readme).toContain('localName')
    expect(readme).toContain('timeoutMs')
    expect(readme).toContain('destroy()')
    expect(readme).toContain('BleManager')
    expect(readme).toContain('ScanSession')
    expect(readme).toContain('Connection')
    expect(readme).toContain('GattDatabase')
    expect(readme).toContain('Subscription')
    expect(teachingLead).not.toMatch(/alpha\.\d+/i)
    expect(readme).not.toMatch(
      /import `unified-ble-manager\/codecs` only when an external protocol requires text encoding/
    )
  })

  test('consumer teaching pages match current public types and do not teach stale claims', () => {
    const consumerGuides = [
      'README.md',
      'MIGRATION_4.0.md',
      'docs/GETTING_STARTED.md',
      'docs/TUTORIALS.md',
      'docs/HELPERS.md',
      'docs/WEB.md',
      'docs/ELECTRON.md',
      'docs/NODE.md',
      'docs/TAURI.md',
      'docs/EXPO_PLUGIN.md',
      'docs/CONNECTION_MANAGER.md'
    ]
    const tutorials = read('docs/TUTORIALS.md')
    const helpers = read('docs/HELPERS.md')
    const gettingStarted = read('docs/GETTING_STARTED.md')
    const web = read('docs/WEB.md')
    const node = read('docs/NODE.md')
    const tauri = read('docs/TAURI.md')

    for (const relativePath of consumerGuides) {
      const document = read(relativePath)
      expect(document).not.toMatch(/encode\/decode explicitly through `unified-ble-manager\/codecs`/)
      expect(document).not.toMatch(/Base64 helpers/)
      expect(document).not.toMatch(/no prebuilt addon/)
    }

    expect(tutorials).not.toMatch(/candidate\.device\.name|observation\.device\.name/)
    expect(tutorials).not.toMatch(/write\(controlPointPath/)
    expect(tutorials).toContain('localName')
    expect(tutorials).toContain('timeoutMs')
    expect(tutorials).not.toContain('deadline(')
    expect(tutorials).not.toContain('capacity(')
    expect(helpers).not.toMatch(/candidate\.device\.name|observation\.device\.name/)
    expect(helpers).not.toMatch(/write\(controlPointPath/)
    expect(helpers).toContain('localName')
    expect(helpers).toContain('timeoutMs')
    expect(helpers).not.toContain('deadline(')
    expect(helpers).not.toContain('capacity(')
    expect(gettingStarted).toContain('createReactNativeBleManager')
    expect(gettingStarted).not.toContain('clientId')
    expect(gettingStarted).not.toContain('managerId')
    expect(gettingStarted).toContain('manager.adapter.state()')
    expect(gettingStarted).toContain(packageVersion)
    expect(web).not.toMatch(/npm'?s `next` tag/)
    expect(web).toContain('ble.choose')
    expect(node).toContain('createBleManagerFromProvider')
    expect(tauri).toContain('createTauriBleManager()')
    expect(tauri).toContain('returns the public `BleManager`')
  })

  test('teaching files that mention 4.0.0-rc. use the current package version', () => {
    const teaching = [
      'README.md',
      'MIGRATION_4.0.md',
      'docs/GETTING_STARTED.md',
      'docs/WEB.md',
      'docs/NODE.md',
      'docs/ELECTRON.md',
      'docs/EXPO_PLUGIN.md',
      'example/README.md'
    ]
    for (const relativePath of teaching) {
      const document = read(relativePath)
      if (document.includes('4.0.0-rc.')) {
        expect(document).toContain(packageVersion)
      }
    }
  })

  test('relative markdown links in teaching files exist', () => {
    const teaching = [
      'README.md',
      'MIGRATION_4.0.md',
      'docs/GETTING_STARTED.md',
      'docs/TUTORIALS.md',
      'docs/HELPERS.md',
      'docs/WEB.md',
      'docs/NODE.md',
      'docs/ELECTRON.md',
      'docs/ELECTRON_SECURITY_MODEL.md',
      'docs/TAURI.md',
      'docs/PROFILES_AND_COMMANDS.md',
      'docs/EXPO_PLUGIN.md'
    ]
    const link = /\[[^\]]*\]\(([^)]+)\)/g
    for (const relativePath of teaching) {
      const document = read(relativePath)
      const fromDirectory = path.dirname(path.join(root, relativePath))
      let match
      while ((match = link.exec(document)) !== null) {
        const target = match[1]
        if (target.startsWith('http:') || target.startsWith('https:') || target.startsWith('mailto:')) {
          continue
        }
        const withoutAnchor = target.split('#')[0]
        if (withoutAnchor.length === 0) {
          continue
        }
        const resolved = path.resolve(fromDirectory, withoutAnchor)
        expect({ relativePath, target, exists: fs.existsSync(resolved) }).toEqual({
          relativePath,
          target,
          exists: true
        })
      }
    }
  })
})
