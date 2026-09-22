// __tests__/TckPackagingProof.test.js
//
// Source contract guard for the TCK test-only packaging proof
// (scripts/ci/check-tck-test-only-packaging.js, findings RV5 1-2).
//
// The `/testing` entry ships TCK content and is consumed by plain-node
// harnesses, so its require-time (static value-import) closure must never
// reach the `react-native` host framework: react-native's Flow entry cannot
// be parsed by plain node, and a static pull breaks every plain-node
// consumer of the built testing entry. Host-framework routes load lazily
// inside their factories (the sanctioned `require` pattern, as in
// src/react-native-app-manager.ts) and are invisible to this check by
// design: only static value imports run at require time.
//
// This guards the source contract; the proof script itself guards the built
// artifact (closure + runtime legs) and CI/preflight wiring below pins that
// the proof actually runs.

const fs = require('fs')
const path = require('path')
const ts = require('typescript')

const root = path.resolve(__dirname, '..')

const SOURCE_EXTENSIONS = ['.ts', '.tsx', '/index.ts', '/index.tsx', '.js', '/index.js']

function repoRelative(file) {
  return path.relative(root, file).split(path.sep).join('/')
}

function resolveFile(candidateBase) {
  const candidates = [candidateBase, ...SOURCE_EXTENSIONS.map(extension => candidateBase + extension)]
  return candidates.find(candidate => fs.existsSync(candidate) && fs.statSync(candidate).isFile()) ?? null
}

/** Static value imports of one file: what plain-node require() evaluates. */
function staticValueImports(file) {
  const content = fs.readFileSync(file, 'utf8')
  const source = ts.createSourceFile(file, content, ts.ScriptTarget.Latest, true)
  const specifiers = []
  for (const statement of source.statements) {
    if (ts.isImportDeclaration(statement)) {
      const clause = statement.importClause
      if (clause !== undefined && (statement.isTypeOnly === true || clause.isTypeOnly === true)) {
        continue
      }
      if (ts.isStringLiteral(statement.moduleSpecifier)) {
        specifiers.push(statement.moduleSpecifier.text)
      }
    } else if (ts.isExportDeclaration(statement)) {
      if (statement.moduleSpecifier === undefined || statement.isTypeOnly === true) {
        continue
      }
      if (ts.isStringLiteral(statement.moduleSpecifier)) {
        specifiers.push(statement.moduleSpecifier.text)
      }
    }
  }
  return specifiers
}

/** Require-time closure of entry: files reachable through static value imports. */
function requireTimeClosure(entry) {
  const visited = new Map()
  const queue = [entry]
  visited.set(entry, null)
  while (queue.length > 0) {
    const file = queue.shift()
    for (const specifier of staticValueImports(file)) {
      if (!specifier.startsWith('.')) {
        continue
      }
      const resolved = resolveFile(path.resolve(path.dirname(file), specifier))
      if (resolved === null) {
        throw new Error(`${repoRelative(file)}: cannot resolve ${specifier}`)
      }
      if (!visited.has(resolved)) {
        visited.set(resolved, file)
        queue.push(resolved)
      }
    }
  }
  return visited
}

describe('testing entry require-time closure', () => {
  const entry = resolveFile(path.join(root, 'src', 'testing.ts'))
  const closure = requireTimeClosure(entry)

  test('reaches the deterministic and TCK registrations without a host framework', () => {
    const reached = new Set([...closure.keys()].map(file => repoRelative(file)))
    for (const expected of [
      'src/testing.ts',
      'src/tck/runner.ts',
      'src/tck/first-party/react-native-tck-registration.ts'
    ]) {
      expect(reached).toContain(expected)
    }
  })

  test('no module in the closure statically imports the react-native host framework', () => {
    const offenders = []
    for (const file of closure.keys()) {
      for (const specifier of staticValueImports(file)) {
        if (specifier === 'react-native' || specifier.startsWith('react-native/')) {
          offenders.push(`${repoRelative(file)} imports ${specifier}`)
        }
      }
    }
    expect(offenders).toEqual([])
  })

  test('the React Native TCK factories stay exported (lazy load, same contract)', () => {
    const content = fs.readFileSync(
      path.join(root, 'src', 'tck', 'first-party', 'react-native-tck-registration.ts'),
      'utf8'
    )
    expect(content).toContain('export function createReactNativeAndroidFirstPartyTckRegistration')
    expect(content).toContain('export function createReactNativeAppleFirstPartyTckRegistration')
    expect(content).toContain("require('../../backends/reactnative/react-native-rust-core-binding')")
    expect(content).toContain("require('../../backends/reactnative/react-native-rust-core-provider')")
  })
})

describe('tck packaging proof wiring', () => {
  test('the package CI job runs the proof next to the G6A proof', () => {
    const workflow = fs.readFileSync(path.join(root, '.github', 'workflows', 'ci.yml'), 'utf8')
    expect(workflow).toContain('node scripts/ci/check-tck-test-only-packaging.js')
  })

  test('preflight runs the proof next to the G6A proof', () => {
    const preflight = fs.readFileSync(path.join(root, 'scripts', 'ci', 'preflight.sh'), 'utf8')
    expect(preflight).toContain('node scripts/ci/check-tck-test-only-packaging.js')
  })

  test('the package CI job checks committed prebuilt freshness on every PR (RV5 finding 4)', () => {
    const workflow = fs.readFileSync(path.join(root, '.github', 'workflows', 'ci.yml'), 'utf8')
    expect(workflow).toContain('pnpm native:status --only android')
  })

  test('the package CI job checks generated artifacts on every PR (RV5 finding 6)', () => {
    const workflow = fs.readFileSync(path.join(root, '.github', 'workflows', 'ci.yml'), 'utf8')
    expect(workflow).toContain('pnpm run docs:check')
  })

  test('Rust trees trigger the native jobs and the release lane runs Apple (RV5 findings 4-5)', () => {
    const workflow = fs.readFileSync(path.join(root, '.github', 'workflows', 'ci.yml'), 'utf8')
    expect(workflow).toContain("'crates/**'")
    expect(workflow).toContain("'bindings/**'")
    expect(workflow).toContain("'vendor/**'")
    expect(workflow).toContain("github.base_ref == '5.0.0'")
  })
})
