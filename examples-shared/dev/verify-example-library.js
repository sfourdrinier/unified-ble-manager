// examples-shared/dev/verify-example-library.js
//
// Finding 241. The Expo example depends on this package as `file:..`, which
// pnpm COPIES at install time, so the copy under
// example-expo/node_modules/unified-ble-manager can lag the repository. Three
// things go stale in it, and only two of them fail loudly:
//
//   - the sealed native identity: the runtime check refuses with
//     protocol.incompatible, correctly and visibly;
//   - the RustCore the pod compiles against: the same, once rebuilt;
//   - the built `lib/` output: NOTHING says anything. The app cannot resolve
//     the package, never finishes loading, and no error reaches Metro, the
//     device console or the driver. It simply never appears.
//
// build-tv.sh verifies the staged copy for the TV path (finding 176). This is
// the same check for the phone path, and it names the silent case first.
//
// Usage: node examples-shared/dev/verify-example-library.js [exampleDir]

'use strict'

const fs = require('fs')
const path = require('path')

const REFRESH = [
  'Refresh it:',
  '  pnpm run prepack                      # build lib/ first, or the copy has no build output',
  '  rm -rf <example>/node_modules/unified-ble-manager',
  '  NODE_OPTIONS=--max-old-space-size=8192 pnpm --dir <example> install --no-frozen-lockfile',
  '',
  'The larger heap is not optional: the copy includes the Apple RustCore',
  'xcframework and pnpm runs out of memory at the default limit.'
].join('\n')

/**
 * Compares the facts read from the repository with the facts read from the
 * example's copy. Both sides are passed in so the decision is testable without
 * a real install.
 */
function inspectExampleLibrary({ repo, copy }) {
  if (!copy.present) {
    return Object.freeze({ ok: false, state: 'absent' })
  }
  if (!copy.hasBuiltLib) {
    return Object.freeze({ ok: false, state: 'no-build-output' })
  }
  if (copy.version !== repo.version) {
    return Object.freeze({
      ok: false,
      state: 'version-mismatch',
      repoVersion: repo.version,
      copyVersion: copy.version
    })
  }
  if (copy.identity !== repo.identity) {
    return Object.freeze({ ok: false, state: 'stale-identity' })
  }
  return Object.freeze({ ok: true, state: 'current', version: repo.version })
}

function describeLibraryOutcome(outcome) {
  switch (outcome.state) {
    case 'current':
      return `verify-example-library: the example's copy matches the repository (version ${String(outcome.version)}).`
    case 'absent':
      return `verify-example-library: the example has no copy of unified-ble-manager. Run the install.\n\n${REFRESH}`
    case 'no-build-output':
      return [
        "verify-example-library: the example's copy of unified-ble-manager has no built lib/.",
        '',
        'This is the silent one. The app cannot resolve the package, never finishes',
        'loading, and no error reaches Metro, the device console or the test driver:',
        'the host simply never appears. It happens when the copy is taken while lib/ is',
        'absent — for example during a `prepack`, which cleans before it rebuilds.',
        '',
        REFRESH
      ].join('\n')
    case 'version-mismatch':
      return `verify-example-library: the example's copy is version ${String(outcome.copyVersion)}, the repository is ${String(outcome.repoVersion)}.\n\n${REFRESH}`
    case 'stale-identity':
      return [
        "verify-example-library: the example's copy carries a different sealed native",
        'build identity than the repository.',
        '',
        'Left alone, every operation fails closed at runtime with',
        'protocol.incompatible: react-native-rust-core.native-identity. Rebuilding the',
        'app without refreshing the copy first does not help — the pod compiles the',
        'RustCore from the copy, so the app would embed the stale one.',
        '',
        REFRESH
      ].join('\n')
    default:
      throw new Error(`verify-example-library: unhandled outcome ${String(outcome.state)}`)
  }
}

function readRepoFacts(root) {
  return {
    identity: fs.readFileSync(path.join(root, 'src', 'generated', 'native-build-identity.ts'), 'utf8'),
    version: JSON.parse(fs.readFileSync(path.join(root, 'package.json'), 'utf8')).version
  }
}

function readCopyFacts(exampleDir) {
  const copyRoot = path.join(exampleDir, 'node_modules', 'unified-ble-manager')
  if (!fs.existsSync(path.join(copyRoot, 'package.json'))) {
    return { present: false }
  }
  const identityPath = path.join(copyRoot, 'src', 'generated', 'native-build-identity.ts')
  return {
    present: true,
    identity: fs.existsSync(identityPath) ? fs.readFileSync(identityPath, 'utf8') : null,
    version: JSON.parse(fs.readFileSync(path.join(copyRoot, 'package.json'), 'utf8')).version,
    hasBuiltLib: fs.existsSync(path.join(copyRoot, 'lib', 'module', 'index.js'))
  }
}

function main(argv) {
  const exampleDir = path.resolve(argv[0] ?? process.cwd())
  const root = path.resolve(__dirname, '..', '..')
  const outcome = inspectExampleLibrary({ repo: readRepoFacts(root), copy: readCopyFacts(exampleDir) })
  const message = describeLibraryOutcome(outcome).replace(/<example>/g, path.relative(root, exampleDir) || '.')
  if (outcome.ok) {
    console.log(message)
    return
  }
  console.error(message)
  process.exitCode = 5
}

if (require.main === module) main(process.argv.slice(2))

module.exports = { inspectExampleLibrary, describeLibraryOutcome }
