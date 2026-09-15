// scripts/ci/check-tck-test-only-packaging.js
//
// Packaging PROOF for TCK test-only fault hooks (UBM 5.0 TCK card, review
// follow-up item 5). Replaces the old flag-checking guard with evidence:
//
// 1. The built production entries (lib/commonjs/index.js and
//    lib/commonjs/backend-sdk.js) never reach the test-only fault-hooks
//    module: neither in their static require closure nor by string reference.
// 2. The built entries do not export fault-hook construction at runtime.
// 3. The production entry SOURCES do not reference the fault-hooks module.
//
// Fails closed: any missing build artifact, any reachability hit, or any
// loader failure exits non-zero. Run after `pnpm prepack`:
//   node scripts/ci/check-tck-test-only-packaging.js

const fs = require('fs')
const path = require('path')

const repoRoot = path.resolve(__dirname, '..', '..')
const builtEntries = ['lib/commonjs/index.js', 'lib/commonjs/backend-sdk.js'].map(relative =>
  path.join(repoRoot, relative)
)
const entrySources = ['src/index.ts', 'src/backend-sdk.ts'].map(relative => path.join(repoRoot, relative))
const forbiddenModuleFragment = 'test-only-fault-hooks'
const forbiddenExports = ['createTestOnlyFaultHooks', 'TCK_TEST_ONLY_MARKER', 'assertTestOnlyFaultContext']

function fail(message) {
  console.error(`tck-packaging-proof FAIL: ${message}`)
  process.exit(1)
}

function collectRequireClosure(entryFile) {
  const visited = new Set()
  const queue = [entryFile]
  const requirePattern = /require\(\s*['"]([^'"]+)['"]\s*\)/g
  while (queue.length > 0) {
    const current = queue.pop()
    if (visited.has(current)) {
      continue
    }
    visited.add(current)
    let content
    try {
      content = fs.readFileSync(current, 'utf8')
    } catch (error) {
      fail(`cannot read closure file ${current}: ${error && error.message}`)
    }
    let match = requirePattern.exec(content)
    while (match !== null) {
      const specifier = match[1]
      if (specifier.startsWith('.')) {
        const candidate = resolveRelativeSpecifier(current, specifier)
        if (candidate !== null && !visited.has(candidate)) {
          queue.push(candidate)
        }
      }
      match = requirePattern.exec(content)
    }
    requirePattern.lastIndex = 0
  }
  return [...visited]
}

function resolveRelativeSpecifier(fromFile, specifier) {
  const base = path.resolve(path.dirname(fromFile), specifier)
  const candidates = [`${base}.js`, path.join(base, 'index.js')]
  for (const candidate of candidates) {
    if (fs.existsSync(candidate)) {
      return candidate
    }
  }
  if (fs.existsSync(base)) {
    return base
  }
  return null
}

for (const entry of builtEntries) {
  if (!fs.existsSync(entry)) {
    fail(`missing built entry ${path.relative(repoRoot, entry)} (run pnpm prepack first)`)
  }
}

for (const entry of builtEntries) {
  const closure = collectRequireClosure(entry)
  const reachable = closure.filter(file => file.includes(forbiddenModuleFragment))
  if (reachable.length > 0) {
    fail(
      `fault-hooks module reachable from ${path.relative(repoRoot, entry)}: ` +
        reachable.map(file => path.relative(repoRoot, file)).join(', ')
    )
  }
  for (const file of closure) {
    const content = fs.readFileSync(file, 'utf8')
    if (content.includes(forbiddenModuleFragment) || content.includes('TCK_TEST_ONLY_MARKER')) {
      fail(`fault-hooks reference inside closure file ${path.relative(repoRoot, file)}`)
    }
  }
  console.log(
    `tck-packaging-proof: ${path.relative(repoRoot, entry)} closure clean (${closure.length} modules, no fault-hooks reachability)`
  )
}

for (const entry of builtEntries) {
  let loaded
  try {
    loaded = require(entry)
  } catch (error) {
    fail(`cannot load built entry ${path.relative(repoRoot, entry)}: ${error && error.message}`)
  }
  if (typeof loaded !== 'object' || loaded === null) {
    fail(`built entry ${path.relative(repoRoot, entry)} did not export an inspectable object`)
  }
  for (const name of forbiddenExports) {
    if (name in loaded) {
      fail(`built entry ${path.relative(repoRoot, entry)} exports forbidden ${name}`)
    }
  }
  console.log(`tck-packaging-proof: ${path.relative(repoRoot, entry)} exports clean at runtime`)
}

for (const source of entrySources) {
  if (!fs.existsSync(source)) {
    fail(`missing entry source ${path.relative(repoRoot, source)}`)
  }
  const content = fs.readFileSync(source, 'utf8')
  if (content.includes(forbiddenModuleFragment)) {
    fail(`entry source ${path.relative(repoRoot, source)} references the fault-hooks module`)
  }
  console.log(`tck-packaging-proof: ${path.relative(repoRoot, source)} source clean`)
}

console.log('tck-packaging-proof PASS')
