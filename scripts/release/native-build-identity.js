#!/usr/bin/env node
// scripts/release/native-build-identity.js
//
// PR210-18: the one implementation of native build identity. Every other
// producer or checker (the Apple and Android builders, the pod script phase,
// Gradle, the build fingerprint, the publish gate, the generated TypeScript
// the runtime compares against) calls this file. Nothing else computes these
// digests.
//
//   sourceDigest(binding)  sha256 over sorted `relpath\0sha256(content)\n`
//                          lines for the binding crate and every local crate
//                          it links: path dependencies, workspace-inherited
//                          path dependencies and [patch.*] path crates (the
//                          vendored btleplug), resolved from Cargo.toml like
//                          cargo does (normal + build dependencies of every
//                          target, optional ones only when an enabled feature
//                          activates them, features unified across the
//                          graph; never dev-dependencies). Each crate
//                          contributes Cargo.toml, its build script (explicit
//                          `build =` or an implicit root build.rs) and
//                          src/**; plus the workspace Cargo.toml, Cargo.lock,
//                          rust-toolchain.toml and the shared build-script
//                          helper bindings/ubm_build_identity.rs.
//   bindingSchema(binding) the same digest over the wrapper-side declaration
//                          files that must match the compiled binary (T1):
//                            napi   bindings/napi/src/{dispatch,lib}.rs
//                            uniffi the UDL + generated Swift surface
//                            jni    bindings/jni/src/lib.rs + every Java class
//                                   named by an exported Java_* symbol
//
// Packed consumers ship this file (and only this file of scripts/): the
// source-mode builders and the pod phase call it from inside node_modules,
// so it must depend on Node built-ins only.
//
// Usage:
//   node scripts/release/native-build-identity.js [--root <dir>] <command>
//     --write                     regenerate src/generated/native-build-identity.ts
//     --check                     fail when that module is stale (prepack)
//     --print-env <binding>       print UBM_BUILD_SOURCE_DIGEST / UBM_BUILD_BINDING_SCHEMA
//                                 for cargo (combinable with --write)
//     --inputs <binding>          print every input path (Gradle input graph)
//     --check-apple [--dir <d>]   verify ios/RustCore: hash chain + current digests
//     --check-android-prebuilts   verify committed jniLibs: hash chain + current digests
//     --write-apple-identity --dir <d> --plist-json <f> --source-digest <h>
//         --binding-schema <h> --profile <p> --toolchain <s> --xcodebuild <s>
//     --write-android-identity --dir <d> --source-digest <h> --binding-schema <h>
//         --profile <p> --toolchain <s> --ndk <s>

'use strict'

const crypto = require('crypto')
const fs = require('fs')
const path = require('path')

const IDENTITY_SCHEMA = 'ubm-native-build-identity/1'
const APPLE_STAGING_SCHEMA = 'ubm-apple-rustcore-identity/1'
const ANDROID_PREBUILT_SCHEMA = 'ubm-android-jnilibs-identity/1'
const GENERATED_RELATIVE = 'src/generated/native-build-identity.ts'
const UNSEALED = 'unsealed'

const WORKSPACE_INPUTS = Object.freeze([
  'Cargo.toml',
  'Cargo.lock',
  'rust-toolchain.toml',
  'bindings/ubm_build_identity.rs'
])

// Declared targets per binding: the runtime check requires the binary's
// `target` to be one of these. The Apple/Android sets must equal what the
// builders produce (asserted by tests against the builder scripts).
const APPLE_TARGETS = Object.freeze([
  'aarch64-apple-ios',
  'aarch64-apple-ios-sim',
  'aarch64-apple-tvos',
  'aarch64-apple-tvos-sim',
  'x86_64-apple-ios'
])

const ANDROID_DECLARED_ABIS = Object.freeze([
  Object.freeze({ abi: 'arm64-v8a', target: 'aarch64-linux-android' }),
  Object.freeze({ abi: 'x86_64', target: 'x86_64-linux-android' })
])

const NAPI_TARGETS = Object.freeze([
  'aarch64-apple-darwin',
  'aarch64-pc-windows-msvc',
  'aarch64-unknown-linux-gnu',
  'x86_64-apple-darwin',
  'x86_64-pc-windows-msvc',
  'x86_64-unknown-linux-gnu'
])

// The exact XCFramework slice set (LibraryIdentifier | SupportedPlatform |
// SupportedPlatformVariant | SupportedArchitectures). ios/verify-rust-core.sh
// carries the same table; a test keeps the two equal.
const APPLE_DECLARED_LIBRARIES = Object.freeze([
  Object.freeze({
    libraryIdentifier: 'ios-arm64',
    platform: 'ios',
    variant: '',
    architectures: Object.freeze(['arm64'])
  }),
  Object.freeze({
    libraryIdentifier: 'ios-arm64_x86_64-simulator',
    platform: 'ios',
    variant: 'simulator',
    architectures: Object.freeze(['arm64', 'x86_64'])
  }),
  Object.freeze({
    libraryIdentifier: 'tvos-arm64',
    platform: 'tvos',
    variant: '',
    architectures: Object.freeze(['arm64'])
  }),
  Object.freeze({
    libraryIdentifier: 'tvos-arm64-simulator',
    platform: 'tvos',
    variant: 'simulator',
    architectures: Object.freeze(['arm64'])
  })
])

const BINDINGS = Object.freeze({
  napi: Object.freeze({ crateDir: 'bindings/napi', targets: NAPI_TARGETS }),
  jni: Object.freeze({
    crateDir: 'bindings/jni',
    targets: Object.freeze(ANDROID_DECLARED_ABIS.map(entry => entry.target))
  }),
  uniffi: Object.freeze({ crateDir: 'bindings/uniffi', targets: APPLE_TARGETS })
})

const BINDING_NAMES = Object.freeze(Object.keys(BINDINGS))

function toPosix(relative) {
  return relative.split(path.sep).join('/')
}

function sha256(content) {
  return crypto.createHash('sha256').update(content).digest('hex')
}

function requireBinding(binding) {
  if (!Object.prototype.hasOwnProperty.call(BINDINGS, binding)) {
    throw new Error(`unknown binding '${binding}' (expected one of ${BINDING_NAMES.join(', ')})`)
  }
  return BINDINGS[binding]
}

function readRequired(root, relative, why) {
  const absolute = path.join(root, relative)
  try {
    return fs.readFileSync(absolute)
  } catch (error) {
    throw new Error(`native-build-identity: missing ${relative} (${why}): ${error.message}`)
  }
}

// --- Cargo.toml (the subset this workspace uses, parsed fail-closed) ---

// Tracks quotes so `#` inside strings and brackets inside strings are ignored.
function scanOutsideStrings(line, onCharacter) {
  let quote = null
  for (let index = 0; index < line.length; index += 1) {
    const character = line[index]
    if (quote !== null) {
      if (character === '\\' && quote === '"') index += 1
      else if (character === quote) quote = null
      continue
    }
    if (character === '"' || character === "'") quote = character
    else if (onCharacter(character, index) === false) return
  }
}

function stripComment(line) {
  let cut = line.length
  scanOutsideStrings(line, (character, index) => {
    if (character !== '#') return true
    cut = index
    return false
  })
  return line.slice(0, cut)
}

function bracketDepth(text) {
  let depth = 0
  scanOutsideStrings(text, character => {
    if (character === '[' || character === '{') depth += 1
    else if (character === ']' || character === '}') depth -= 1
    return true
  })
  return depth
}

// Sections with their key/value entries. A value whose arrays or inline
// tables span lines (`features = [\n ... ]`) is joined into one entry.
function parseManifestSections(text) {
  const sections = [{ name: '', entries: [] }]
  let pending = null
  for (const rawLine of text.split(/\r?\n/)) {
    const line = stripComment(rawLine).trim()
    if (pending !== null) {
      pending.value = `${pending.value} ${line}`
      if (bracketDepth(pending.value) <= 0) pending = null
      continue
    }
    if (line === '') continue
    const header = /^\[\[?\s*(.+?)\s*\]\]?$/.exec(line)
    if (header !== null) {
      sections.push({ name: header[1], entries: [] })
      continue
    }
    const entry = /^([A-Za-z0-9_.\-"']+)\s*=\s*(.+)$/.exec(line)
    if (entry === null) continue
    const record = { key: entry[1], value: entry[2] }
    sections[sections.length - 1].entries.push(record)
    if (bracketDepth(record.value) > 0) pending = record
  }
  if (pending !== null) throw new Error(`native-build-identity: unterminated value for ${pending.key}`)
  return sections
}

function unquote(key) {
  return key.replace(/^["']|["']$/g, '')
}

function stringArray(value) {
  return [...value.matchAll(/"([^"]*)"/g)].map(match => match[1])
}

// One dependency spec from an inline table or a `[dependencies.<name>]`
// body (both rendered as `key = value, ...`).
function dependencySpec(body) {
  const pathMatch = /\bpath\s*=\s*"([^"]+)"/.exec(body)
  const packageMatch = /\bpackage\s*=\s*"([^"]+)"/.exec(body)
  const featuresMatch = /\bfeatures\s*=\s*(\[[^\]]*\])/.exec(body)
  return {
    path: pathMatch === null ? null : pathMatch[1],
    package: packageMatch === null ? null : packageMatch[1],
    workspace: /\bworkspace\s*=\s*true\b/.test(body),
    optional: /\boptional\s*=\s*true\b/.test(body),
    defaultFeatures: !/\bdefault[-_]features\s*=\s*false\b/.test(body),
    features: featuresMatch === null ? [] : stringArray(featuresMatch[1])
  }
}

const TARGET_PREFIX = String.raw`(?:target\.(?:'[^']*'|"[^"]*"|[A-Za-z0-9_\-]+)\.)?`
const DEPENDENCY_SECTION = new RegExp(`^${TARGET_PREFIX}(?:dependencies|build-dependencies)$`)
const DEPENDENCY_TABLE = new RegExp(`^${TARGET_PREFIX}(?:dependencies|build-dependencies)\\.(.+)$`)

function mergeSpecs(current, next) {
  if (current === undefined) return next
  return {
    path: next.path ?? current.path,
    package: next.package ?? current.package,
    workspace: current.workspace || next.workspace,
    // A dependency declared in several (target) tables: it is optional only
    // if every declaration is; it keeps default features if any does.
    optional: current.optional && next.optional,
    defaultFeatures: current.defaultFeatures || next.defaultFeatures,
    features: [...new Set([...current.features, ...next.features])]
  }
}

// Parsed manifest: every non-dev dependency keyed by its declared name (the
// name features refer to), the [features] table, and [lib] path.
function parseCrateManifest(text) {
  const dependencies = new Map()
  const features = new Map()
  let libPath = null
  const add = (name, spec) => dependencies.set(name, mergeSpecs(dependencies.get(name), spec))
  for (const section of parseManifestSections(text)) {
    if (DEPENDENCY_SECTION.test(section.name)) {
      const dotted = new Map()
      for (const { key, value } of section.entries) {
        const parts = /^([^.]+)\.(.+)$/.exec(key)
        if (parts !== null) {
          const name = unquote(parts[1])
          dotted.set(name, `${dotted.get(name) ?? ''}${parts[2]} = ${value}, `)
          continue
        }
        add(unquote(key), dependencySpec(value.startsWith('{') ? value : ''))
      }
      for (const [name, body] of dotted) add(name, dependencySpec(body))
      continue
    }
    const table = DEPENDENCY_TABLE.exec(section.name)
    if (table !== null) {
      add(unquote(table[1]), dependencySpec(section.entries.map(({ key, value }) => `${key} = ${value}`).join(', ')))
      continue
    }
    if (section.name === 'features') {
      for (const { key, value } of section.entries) features.set(unquote(key), stringArray(value))
      continue
    }
    if (section.name === 'lib') {
      for (const { key, value } of section.entries) {
        const declared = /^"([^"]+)"$/.exec(value)
        if (key === 'path' && declared !== null) libPath = declared[1]
      }
    }
  }
  return { dependencies, features, libPath }
}

// Workspace-level path sources: [workspace.dependencies] path entries and
// every [patch.<registry>] path entry (a vendored crate replacing a
// registry dependency, e.g. `btleplug = { path = "vendor/btleplug" }`).
function workspacePathSources(root) {
  const text = readRequired(root, 'Cargo.toml', 'workspace manifest').toString('utf8')
  const inherited = new Map()
  const patched = new Map()
  const collect = (target, name, body) => {
    const spec = dependencySpec(body)
    if (spec.path !== null) target.set(unquote(spec.package ?? name), toPosix(path.normalize(spec.path)))
  }
  for (const section of parseManifestSections(text)) {
    for (const [pattern, target] of [
      [/^workspace\.dependencies$/, inherited],
      [/^patch\.(?:'[^']*'|"[^"]*"|[A-Za-z0-9_\-]+)$/, patched]
    ]) {
      if (pattern.test(section.name)) {
        for (const { key, value } of section.entries) collect(target, unquote(key), value.startsWith('{') ? value : '')
      }
    }
    const inheritedTable = /^workspace\.dependencies\.(.+)$/.exec(section.name)
    const patchedTable = /^patch\.(?:'[^']*'|"[^"]*"|[A-Za-z0-9_\-]+)\.(.+)$/.exec(section.name)
    for (const [match, target] of [
      [inheritedTable, inherited],
      [patchedTable, patched]
    ]) {
      if (match !== null) {
        collect(target, match[1], section.entries.map(({ key, value }) => `${key} = ${value}`).join(', '))
      }
    }
  }
  return { inherited, patched }
}

// The local (path, workspace-inherited or [patch]ed) crates the binding
// links, resolved like cargo: optional dependencies count only when an
// enabled feature activates them, and features requested anywhere in the
// graph are unified. Target-specific dependencies count for every target
// (the digest covers every declared target). Registry crates are pinned by
// Cargo.lock, which is itself an input.
function resolvePathDependencies(root, crateRelative) {
  const { inherited, patched } = workspacePathSources(root)
  const manifests = new Map()
  const manifestOf = crateDir => {
    if (!manifests.has(crateDir)) {
      const text = readRequired(root, `${crateDir}/Cargo.toml`, 'crate manifest').toString('utf8')
      manifests.set(crateDir, parseCrateManifest(text))
    }
    return manifests.get(crateDir)
  }
  const localDir = (crateDir, name, spec) => {
    let dir = null
    if (spec.path !== null) dir = toPosix(path.normalize(path.join(crateDir, spec.path)))
    else if (spec.workspace && inherited.has(spec.package ?? name)) dir = inherited.get(spec.package ?? name)
    else if (patched.has(spec.package ?? name)) dir = patched.get(spec.package ?? name)
    if (dir !== null && dir.startsWith('..')) {
      throw new Error(`native-build-identity: path dependency ${name} of ${crateDir} leaves the workspace (${dir})`)
    }
    return dir
  }
  const enabled = new Map()
  const queue = []
  const request = (crateDir, features) => {
    const current = enabled.get(crateDir) ?? new Set()
    const before = current.size
    const isNew = !enabled.has(crateDir)
    features.forEach(feature => current.add(feature))
    enabled.set(crateDir, current)
    if (isNew || current.size !== before) queue.push(crateDir)
  }
  request(crateRelative, ['default'])
  while (queue.length > 0) {
    const crateDir = queue.shift()
    const manifest = manifestOf(crateDir)
    const features = enabled.get(crateDir)
    const activated = new Set()
    const forwarded = new Map()
    const weak = []
    const expand = [...features]
    for (let index = 0; index < expand.length; index += 1) {
      const feature = expand[index]
      if (manifest.features.has(feature)) {
        for (const item of manifest.features.get(feature)) {
          const dep = /^dep:(.+)$/.exec(item)
          const sub = /^([^/?]+)(\?)?\/(.+)$/.exec(item)
          if (dep !== null) activated.add(dep[1])
          else if (sub !== null) {
            if (sub[2] === '?') weak.push([sub[1], sub[3]])
            else activated.add(sub[1])
            forwarded.set(sub[1], [...(forwarded.get(sub[1]) ?? []), sub[3]])
          } else if (!features.has(item)) {
            features.add(item)
            expand.push(item)
          }
        }
      } else if (manifest.dependencies.has(feature) && manifest.dependencies.get(feature).optional) {
        activated.add(feature) // implicit feature of an optional dependency
      }
    }
    for (const [name, spec] of manifest.dependencies) {
      if (spec.optional && !activated.has(name)) continue
      const dir = localDir(crateDir, name, spec)
      if (dir === null) continue
      const requested = [...spec.features, ...(forwarded.get(name) ?? [])]
      if (spec.defaultFeatures) requested.push('default')
      request(dir, requested)
    }
    for (const [name] of weak) {
      if (!manifest.dependencies.has(name)) {
        throw new Error(`native-build-identity: feature of ${crateDir} names unknown dependency ${name}`)
      }
    }
  }
  enabled.delete(crateRelative)
  return [...enabled.keys()].sort()
}

function walkFiles(root, relativeDir) {
  const files = []
  const absoluteDir = path.join(root, relativeDir)
  for (const entry of fs.readdirSync(absoluteDir, { withFileTypes: true })) {
    if (entry.name.startsWith('.')) continue
    const relative = `${relativeDir}/${entry.name}`
    if (entry.isDirectory()) files.push(...walkFiles(root, relative))
    else if (entry.isFile()) files.push(relative)
    else
      throw new Error(
        `native-build-identity: ${relative} is neither a file nor a directory (symlinks are not build inputs)`
      )
  }
  return files
}

function crateInputs(root, crateDir) {
  const manifestRelative = `${crateDir}/Cargo.toml`
  const text = readRequired(root, manifestRelative, 'crate manifest').toString('utf8')
  const inputs = [manifestRelative]
  let buildScript = 'build.rs'
  for (const section of parseManifestSections(text)) {
    if (section.name !== 'package') continue
    for (const { key, value } of section.entries) {
      if (key !== 'build') continue
      if (value === 'false') buildScript = null
      else {
        const declared = /^"([^"]+)"$/.exec(value)
        if (declared === null)
          throw new Error(`native-build-identity: unsupported build = ${value} in ${manifestRelative}`)
        buildScript = declared[1]
        readRequired(root, `${crateDir}/${buildScript}`, 'declared build script')
      }
    }
  }
  if (buildScript !== null && fs.existsSync(path.join(root, crateDir, buildScript))) {
    inputs.push(`${crateDir}/${buildScript}`)
  }
  if (!fs.existsSync(path.join(root, crateDir, 'src'))) {
    throw new Error(`native-build-identity: crate ${crateDir} has no src/ directory`)
  }
  inputs.push(...walkFiles(root, `${crateDir}/src`))
  // A [lib] path outside src/ is a build input too; anything else outside
  // src/ it reaches (#[path], include!) would be missed, so fail closed.
  const { libPath } = parseCrateManifest(text)
  if (libPath !== null && !toPosix(path.normalize(libPath)).startsWith('src/')) {
    throw new Error(
      `native-build-identity: ${manifestRelative} declares [lib] path = "${libPath}" outside src/ (unsupported input layout)`
    )
  }
  return inputs
}

function bindingSourceInputs(root, binding) {
  const { crateDir } = requireBinding(binding)
  const inputs = new Set(WORKSPACE_INPUTS)
  for (const dir of [crateDir, ...resolvePathDependencies(root, crateDir)]) {
    for (const input of crateInputs(root, dir)) inputs.add(input)
  }
  return [...inputs].sort()
}

function jniSchemaInputs(root) {
  const inputs = new Set(['bindings/jni/src/lib.rs'])
  const classes = new Set()
  for (const relative of walkFiles(root, 'bindings/jni/src')) {
    const text = fs.readFileSync(path.join(root, relative), 'utf8')
    for (const match of text.matchAll(/\bJava_([A-Za-z0-9_]+)/g)) {
      const segments = match[1].split('_')
      const classIndex = segments.findIndex(segment => /^[A-Z]/.test(segment))
      if (classIndex <= 0) {
        throw new Error(
          `native-build-identity: cannot resolve the Java class of JNI symbol Java_${match[1]} in ${relative}`
        )
      }
      classes.add([...segments.slice(0, classIndex), segments[classIndex]].join('/'))
    }
  }
  for (const javaClass of [...classes].sort()) {
    const candidates = [`android/src/main/java/${javaClass}.java`, `android/src/main/java/${javaClass}.kt`]
    const shipped = candidates.filter(candidate => fs.existsSync(path.join(root, candidate)))
    if (shipped.length === 0) {
      throw new Error(`native-build-identity: JNI class ${javaClass} has no declaration at ${candidates.join(' or ')}`)
    }
    shipped.forEach(file => inputs.add(file))
    const owner = `bindings/jni/java/${javaClass}.java`
    if (fs.existsSync(path.join(root, owner))) inputs.add(owner)
  }
  return inputs
}

function bindingSchemaInputs(root, binding) {
  requireBinding(binding)
  let inputs
  if (binding === 'napi') {
    inputs = ['bindings/napi/src/dispatch.rs', 'bindings/napi/src/lib.rs']
  } else if (binding === 'uniffi') {
    inputs = [
      'bindings/uniffi/src/ubm_echo.udl',
      'bindings/uniffi/generated/swift/ubm_echo.swift',
      'bindings/uniffi/generated/swift/ubm_echoFFI.h',
      'bindings/uniffi/generated/swift/ubm_echoFFI.modulemap'
    ]
  } else {
    inputs = [...jniSchemaInputs(root)]
  }
  return inputs.sort()
}

function digestFiles(root, relatives, why) {
  const lines = [...relatives]
    .sort()
    .map(relative => `${relative}\0${sha256(readRequired(root, relative, why))}\n`)
    .join('')
  return sha256(lines)
}

function computeBindingIdentity(root, binding) {
  return {
    sourceDigest: digestFiles(root, bindingSourceInputs(root, binding), `${binding} source input`),
    bindingSchema: digestFiles(root, bindingSchemaInputs(root, binding), `${binding} binding schema input`)
  }
}

function readContractRevision(root) {
  const text = readRequired(root, 'crates/ubm-core/src/contracts.rs', 'contract revision').toString('utf8')
  const match = /pub const CONTRACT_REVISION:\s*&str\s*=\s*"([^"]+)"/.exec(text)
  if (match === null)
    throw new Error('native-build-identity: CONTRACT_REVISION not found in crates/ubm-core/src/contracts.rs')
  return match[1]
}

function computeNativeBuildIdentity(root) {
  const bindings = {}
  for (const binding of BINDING_NAMES) {
    bindings[binding] = { ...computeBindingIdentity(root, binding), targets: [...BINDINGS[binding].targets] }
  }
  return { schema: IDENTITY_SCHEMA, contractRevision: readContractRevision(root), bindings }
}

// --- generated TypeScript ---

function tsString(value) {
  return `'${value.replace(/\\/g, '\\\\').replace(/'/g, "\\'")}'`
}

// Prettier-stable (printWidth 120): one line when it fits, else one per target.
function renderTargets(targets) {
  const single = `      targets: Object.freeze([${targets.map(tsString).join(', ')}])`
  if (single.length <= 120) return single
  return [
    '      targets: Object.freeze([',
    targets.map(target => `        ${tsString(target)}`).join(',\n'),
    '      ])'
  ].join('\n')
}

function renderGeneratedModule(computed) {
  const bindingBlocks = BINDING_NAMES.map(binding => {
    const entry = computed.bindings[binding]
    return [
      `    ${binding}: Object.freeze({`,
      `      sourceDigest: ${tsString(entry.sourceDigest)},`,
      `      bindingSchema: ${tsString(entry.bindingSchema)},`,
      renderTargets(entry.targets),
      '    })'
    ].join('\n')
  }).join(',\n')
  return `// ${GENERATED_RELATIVE}
//
// @generated by scripts/release/native-build-identity.js --write. Do not edit.
// \`node scripts/release/native-build-identity.js --check\` (run by prepack)
// fails when this module no longer matches the Rust sources. The runtime
// compares each native binary's build_identity_json() against these values
// before any radio call.

export type NativeBuildBinding = ${BINDING_NAMES.map(tsString).join(' | ')}

export interface ExpectedNativeBindingIdentity {
  readonly sourceDigest: string
  readonly bindingSchema: string
  readonly targets: readonly string[]
}

export interface ExpectedNativeBuildIdentity {
  readonly schema: ${tsString(IDENTITY_SCHEMA)}
  readonly contractRevision: string
  readonly bindings: Readonly<Record<NativeBuildBinding, ExpectedNativeBindingIdentity>>
}

export const EXPECTED_NATIVE_BUILD_IDENTITY: ExpectedNativeBuildIdentity = Object.freeze({
  schema: ${tsString(IDENTITY_SCHEMA)},
  contractRevision: ${tsString(computed.contractRevision)},
  bindings: Object.freeze({
${bindingBlocks}
  })
})
`
}

function writeGenerated(root) {
  const target = path.join(root, GENERATED_RELATIVE)
  const rendered = renderGeneratedModule(computeNativeBuildIdentity(root))
  fs.mkdirSync(path.dirname(target), { recursive: true })
  if (!fs.existsSync(target) || fs.readFileSync(target, 'utf8') !== rendered) fs.writeFileSync(target, rendered)
  return target
}

function checkGenerated(root) {
  const target = path.join(root, GENERATED_RELATIVE)
  const rerun = `node ${path.join(root, 'scripts', 'release', 'native-build-identity.js')} --write`
  if (!fs.existsSync(target)) {
    throw new Error(`native-build-identity: ${GENERATED_RELATIVE} is missing. Generate it:\n  ${rerun}`)
  }
  const rendered = renderGeneratedModule(computeNativeBuildIdentity(root))
  if (fs.readFileSync(target, 'utf8') !== rendered) {
    throw new Error(
      `native-build-identity: ${GENERATED_RELATIVE} is stale (Rust sources, binding schema or contract revision changed). Regenerate it and rebuild the native artifacts:\n  ${rerun}`
    )
  }
  return true
}

// --- staging hash chains ---

function hexOrThrow(value, name) {
  if (typeof value !== 'string' || !/^[0-9a-f]{64}$/.test(value)) {
    throw new Error(`native-build-identity: ${name} must be 64 lowercase hex characters, got ${JSON.stringify(value)}`)
  }
  return value
}

function readJsonRecord(file, missingMessage) {
  let text
  try {
    text = fs.readFileSync(file, 'utf8')
  } catch {
    throw new Error(missingMessage)
  }
  try {
    return JSON.parse(text)
  } catch (error) {
    throw new Error(`native-build-identity: ${file} is not valid JSON: ${error.message}`)
  }
}

function compareDigests(root, binding, record, rerun) {
  const current = computeBindingIdentity(root, binding)
  const differing = []
  for (const field of ['sourceDigest', 'bindingSchema']) {
    if (record[field] !== current[field]) {
      differing.push(`${field}: staged ${JSON.stringify(record[field])}, sources ${current[field]}`)
    }
  }
  if (record.contractRevision !== readContractRevision(root)) {
    differing.push(
      `contractRevision: staged ${JSON.stringify(record.contractRevision)}, sources ${readContractRevision(root)}`
    )
  }
  if (differing.length > 0) {
    throw new Error(
      `native-build-identity: staged ${binding} artifacts were not built from these sources:\n  ${differing.join('\n  ')}\nRebuild them:\n  ${rerun}`
    )
  }
}

function libraryKey(entry) {
  const architectures = Array.isArray(entry.architectures) ? [...entry.architectures].sort().join(',') : ''
  return `${entry.libraryIdentifier}|${entry.platform}|${entry.variant}|${architectures}`
}

function appleRerun(root) {
  return `UBM_NATIVE_BUILD=source pnpm --dir ${root} native:apple:prepare   (or: sh ${path.join(root, 'ios', 'build-rust-core.sh')})`
}

function checkAppleStaging(root, dir = path.join(root, 'ios', 'RustCore')) {
  const rerun = appleRerun(root)
  const identityFile = path.join(dir, 'build-identity.json')
  const record = readJsonRecord(
    identityFile,
    `native-build-identity: missing ${identityFile}. Stage the Apple Rust core:\n  ${rerun}`
  )
  if (record.schema !== APPLE_STAGING_SCHEMA || record.binding !== 'uniffi') {
    throw new Error(
      `native-build-identity: ${identityFile} is not an ${APPLE_STAGING_SCHEMA} uniffi record. Restage it:\n  ${rerun}`
    )
  }
  const framework = path.join(dir, 'RustCore.xcframework')
  const plist = path.join(framework, 'Info.plist')
  if (!fs.existsSync(plist)) throw new Error(`native-build-identity: missing ${plist}. Restage it:\n  ${rerun}`)
  if (sha256(fs.readFileSync(plist)) !== hexOrThrow(record.infoPlistSha256, 'infoPlistSha256')) {
    throw new Error(
      `native-build-identity: ${plist} sha256 does not match build-identity.json. Restage it:\n  ${rerun}`
    )
  }
  const libraries = Array.isArray(record.libraries) ? record.libraries : []
  const staged = libraries.map(libraryKey).sort()
  const declared = APPLE_DECLARED_LIBRARIES.map(libraryKey).sort()
  if (JSON.stringify(staged) !== JSON.stringify(declared)) {
    throw new Error(
      `native-build-identity: staged XCFramework does not match the declared slice set.\n  staged:   ${staged.join(' ; ')}\n  declared: ${declared.join(' ; ')}\nRestage it:\n  ${rerun}`
    )
  }
  const expectedArchives = new Set()
  for (const library of libraries) {
    if (
      typeof library.libraryPath !== 'string' ||
      library.libraryPath.includes('/') ||
      library.libraryPath.startsWith('.')
    ) {
      throw new Error(
        `native-build-identity: invalid libraryPath ${JSON.stringify(library.libraryPath)} for ${library.libraryIdentifier}`
      )
    }
    const relative = `${library.libraryIdentifier}/${library.libraryPath}`
    expectedArchives.add(relative)
    const archive = path.join(framework, library.libraryIdentifier, library.libraryPath)
    if (!fs.existsSync(archive))
      throw new Error(`native-build-identity: missing slice ${relative}. Restage it:\n  ${rerun}`)
    const content = fs.readFileSync(archive)
    if (sha256(content) !== hexOrThrow(library.sha256, `${relative} sha256`) || content.length !== library.bytes) {
      throw new Error(
        `native-build-identity: slice ${relative} does not match build-identity.json (substituted or corrupted). Restage it:\n  ${rerun}`
      )
    }
  }
  const frameworkRelative = toPosix(path.relative(root, framework))
  for (const archive of walkFiles(root, frameworkRelative)) {
    const inside = archive.slice(frameworkRelative.length + 1)
    if (archive.endsWith('.a') && !expectedArchives.has(inside)) {
      throw new Error(
        `native-build-identity: undeclared archive ${inside} inside the XCFramework. Restage it:\n  ${rerun}`
      )
    }
  }
  compareDigests(root, 'uniffi', record, rerun)
  return true
}

function androidRerun(root) {
  return `sh ${path.join(root, 'android', 'refresh-prebuilt-jniLibs.sh')}   (maintainer step; commit the refreshed tree)`
}

function checkAndroidPrebuilts(root) {
  const rerun = androidRerun(root)
  const dir = path.join(root, 'android', 'src', 'main', 'jniLibs')
  const identityFile = path.join(dir, 'build-identity.json')
  const record = readJsonRecord(
    identityFile,
    `native-build-identity: missing ${identityFile}. Refresh the prebuilts:\n  ${rerun}`
  )
  if (record.schema !== ANDROID_PREBUILT_SCHEMA || record.binding !== 'jni') {
    throw new Error(
      `native-build-identity: ${identityFile} is not an ${ANDROID_PREBUILT_SCHEMA} jni record. Refresh:\n  ${rerun}`
    )
  }
  const abis = Array.isArray(record.abis) ? record.abis : []
  const staged = abis.map(entry => `${entry.abi}|${entry.target}`).sort()
  const declared = ANDROID_DECLARED_ABIS.map(entry => `${entry.abi}|${entry.target}`).sort()
  if (JSON.stringify(staged) !== JSON.stringify(declared)) {
    throw new Error(
      `native-build-identity: committed ABIs ${staged.join(', ')} differ from the declared set ${declared.join(', ')}. Refresh:\n  ${rerun}`
    )
  }
  for (const entry of abis) {
    if (typeof entry.file !== 'string' || entry.file.includes('/')) {
      throw new Error(`native-build-identity: invalid file ${JSON.stringify(entry.file)} for ABI ${entry.abi}`)
    }
    const library = path.join(dir, entry.abi, entry.file)
    if (!fs.existsSync(library))
      throw new Error(`native-build-identity: missing ${entry.abi}/${entry.file}. Refresh:\n  ${rerun}`)
    const content = fs.readFileSync(library)
    if (sha256(content) !== hexOrThrow(entry.sha256, `${entry.abi} sha256`) || content.length !== entry.bytes) {
      throw new Error(
        `native-build-identity: ${entry.abi}/${entry.file} does not match build-identity.json (substituted or corrupted). Refresh:\n  ${rerun}`
      )
    }
  }
  if (record.sourceDigest === null || record.bindingSchema === null) {
    throw new Error(
      `native-build-identity: committed jniLibs carry no sourceDigest/bindingSchema${
        typeof record.provenance === 'string' ? ` (${record.provenance})` : ''
      }. Refresh them:\n  ${rerun}`
    )
  }
  compareDigests(root, 'jni', record, rerun)
  return true
}

// --- identity writers (called by the canonical builders) ---

function writeJson(file, value) {
  fs.writeFileSync(`${file}.tmp`, `${JSON.stringify(value, null, 2)}\n`)
  fs.renameSync(`${file}.tmp`, file)
}

function requiredOption(options, name) {
  const value = options[name]
  if (typeof value !== 'string' || value === '') throw new Error(`--${name} is required`)
  return value
}

function writeAppleIdentity(root, options) {
  const dir = path.resolve(requiredOption(options, 'dir'))
  const plistJson = JSON.parse(fs.readFileSync(requiredOption(options, 'plist-json'), 'utf8'))
  const framework = path.join(dir, 'RustCore.xcframework')
  const available = Array.isArray(plistJson.AvailableLibraries) ? plistJson.AvailableLibraries : []
  if (available.length === 0) throw new Error('native-build-identity: Info.plist declares no AvailableLibraries')
  const libraries = available.map(library => {
    const archive = path.join(framework, library.LibraryIdentifier, library.LibraryPath)
    const content = fs.readFileSync(archive)
    return {
      libraryIdentifier: library.LibraryIdentifier,
      platform: library.SupportedPlatform,
      variant: typeof library.SupportedPlatformVariant === 'string' ? library.SupportedPlatformVariant : '',
      architectures: [...library.SupportedArchitectures].sort(),
      libraryPath: library.LibraryPath,
      sha256: sha256(content),
      bytes: content.length
    }
  })
  libraries.sort((left, right) => left.libraryIdentifier.localeCompare(right.libraryIdentifier))
  writeJson(path.join(dir, 'build-identity.json'), {
    schema: APPLE_STAGING_SCHEMA,
    binding: 'uniffi',
    contractRevision: readContractRevision(root),
    sourceDigest: hexOrThrow(requiredOption(options, 'source-digest'), '--source-digest'),
    bindingSchema: hexOrThrow(requiredOption(options, 'binding-schema'), '--binding-schema'),
    profile: requiredOption(options, 'profile'),
    toolchain: requiredOption(options, 'toolchain'),
    xcodebuild: requiredOption(options, 'xcodebuild'),
    infoPlistSha256: sha256(fs.readFileSync(path.join(framework, 'Info.plist'))),
    libraries
  })
  return checkAppleStaging(root, dir)
}

function writeAndroidIdentity(root, options) {
  const dir = path.resolve(requiredOption(options, 'dir'))
  const abis = ANDROID_DECLARED_ABIS.map(({ abi, target }) => {
    const file = 'libubm5_jni_echo.so'
    const content = fs.readFileSync(path.join(dir, abi, file))
    return { abi, target, file, sha256: sha256(content), bytes: content.length }
  })
  writeJson(path.join(dir, 'build-identity.json'), {
    schema: ANDROID_PREBUILT_SCHEMA,
    binding: 'jni',
    contractRevision: readContractRevision(root),
    sourceDigest: hexOrThrow(requiredOption(options, 'source-digest'), '--source-digest'),
    bindingSchema: hexOrThrow(requiredOption(options, 'binding-schema'), '--binding-schema'),
    profile: requiredOption(options, 'profile'),
    toolchain: requiredOption(options, 'toolchain'),
    ndk: requiredOption(options, 'ndk'),
    abis
  })
  return true
}

// --- CLI ---

const VALUE_OPTIONS = new Set([
  'root',
  'print-env',
  'inputs',
  'dir',
  'plist-json',
  'source-digest',
  'binding-schema',
  'profile',
  'toolchain',
  'xcodebuild',
  'ndk'
])
const FLAG_OPTIONS = new Set([
  'write',
  'check',
  'check-apple',
  'check-android-prebuilts',
  'write-apple-identity',
  'write-android-identity'
])

function parseArguments(argv) {
  const options = { root: path.resolve(__dirname, '..', '..') }
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index]
    const name = argument.startsWith('--') ? argument.slice(2) : null
    if (name !== null && FLAG_OPTIONS.has(name)) {
      options[name] = true
      continue
    }
    if (name !== null && VALUE_OPTIONS.has(name)) {
      const value = argv[index + 1]
      if (value === undefined) throw new Error(`${argument} requires a value`)
      options[name] = value
      index += 1
      continue
    }
    throw new Error(`Unknown argument: ${argument}`)
  }
  options.root = path.resolve(options.root)
  return options
}

function main(argv) {
  const options = parseArguments(argv)
  const { root } = options
  const out = []
  let acted = false
  if (options.write) {
    writeGenerated(root)
    acted = true
  }
  if (options.check) {
    checkGenerated(root)
    out.push(`native-build-identity: current (${GENERATED_RELATIVE})`)
    acted = true
  }
  if (options['print-env'] !== undefined) {
    const current = computeBindingIdentity(root, options['print-env'])
    out.push(`UBM_BUILD_SOURCE_DIGEST=${current.sourceDigest}`, `UBM_BUILD_BINDING_SCHEMA=${current.bindingSchema}`)
    acted = true
  }
  if (options.inputs !== undefined) {
    const binding = options.inputs
    const inputs = new Set([...bindingSourceInputs(root, binding), ...bindingSchemaInputs(root, binding)])
    out.push(...[...inputs].sort().map(relative => path.join(root, relative)))
    acted = true
  }
  if (options['check-apple']) {
    checkAppleStaging(root, options.dir === undefined ? undefined : path.resolve(options.dir))
    out.push('native-build-identity: Apple RustCore staging verified (hash chain + current sources)')
    acted = true
  }
  if (options['check-android-prebuilts']) {
    checkAndroidPrebuilts(root)
    out.push('native-build-identity: committed Android jniLibs verified (hash chain + current sources)')
    acted = true
  }
  if (options['write-apple-identity']) {
    writeAppleIdentity(root, options)
    out.push(`native-build-identity: wrote + verified ${path.join(path.resolve(options.dir), 'build-identity.json')}`)
    acted = true
  }
  if (options['write-android-identity']) {
    writeAndroidIdentity(root, options)
    out.push(`native-build-identity: wrote ${path.join(path.resolve(options.dir), 'build-identity.json')}`)
    acted = true
  }
  if (!acted) throw new Error('no command given (see the usage header of scripts/release/native-build-identity.js)')
  return out
}

if (require.main === module) {
  try {
    const lines = main(process.argv.slice(2))
    if (lines.length > 0) process.stdout.write(`${lines.join('\n')}\n`)
  } catch (error) {
    process.stderr.write(`${error && error.message ? error.message : String(error)}\n`)
    process.exitCode = 1
  }
}

module.exports = {
  IDENTITY_SCHEMA,
  APPLE_STAGING_SCHEMA,
  ANDROID_PREBUILT_SCHEMA,
  GENERATED_RELATIVE,
  UNSEALED,
  BINDING_NAMES,
  BINDINGS,
  APPLE_DECLARED_LIBRARIES,
  ANDROID_DECLARED_ABIS,
  resolvePathDependencies,
  bindingSourceInputs,
  bindingSchemaInputs,
  computeBindingIdentity,
  computeNativeBuildIdentity,
  readContractRevision,
  renderGeneratedModule,
  writeGenerated,
  checkGenerated,
  checkAppleStaging,
  checkAndroidPrebuilts
}
