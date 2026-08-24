'use strict'

const fs = require('node:fs')
const path = require('node:path')
const ts = require('typescript')

const root = path.join(__dirname, '..', '..')
const writeReports = process.argv.slice(2).includes('--write')

// TypeScript package exports require a committed reviewed report. Metadata and
// config-plugin entrypoints are package surfaces, but are not TypeScript API
// modules and are intentionally checked by their own package/plugin gates.
const typeScriptEntrypoints = Object.freeze({
  '.': 'src/index.ts',
  './backend-sdk': 'src/backend-sdk.ts',
  './cli': 'src/cli.ts',
  './testing': 'src/testing.ts',
  './codecs': 'src/codecs.ts',
  './profiles/commands': 'src/profiles/commands.ts',
  './profiles/standard-commands': 'src/profiles/standard-commands.ts',
  './profiles/heart-rate': 'src/profiles/heart-rate.ts',
  './profiles/battery-service': 'src/profiles/battery-service.ts',
  './profiles/device-information': 'src/profiles/device-information.ts',
  './profiles/health-thermometer': 'src/profiles/health-thermometer.ts',
  './profiles/blood-pressure': 'src/profiles/blood-pressure.ts',
  './profiles/ieee-11073': 'src/profiles/ieee-11073.ts',
  './web': 'src/web.ts',
  './react-native': 'src/react-native.ts',
  './react': 'src/react.ts',
  './node/bluez': 'src/node-bluez.ts',
  './node/corebluetooth': 'src/node-corebluetooth.ts',
  './node/winrt': 'src/node-winrt.ts',
  './electron/main': 'src/electron-main.ts',
  './electron/renderer': 'src/electron-renderer.ts',
  './tauri': 'src/tauri.ts',
  './advanced': 'src/advanced.ts',
  './expo': 'src/expo.ts'
})

const nonTypeScriptEntrypoints = Object.freeze(['./app.plugin.js', './package.json'])
const reportMarker = '\n## Verified exported symbols\n'

function reportPath(entrypoint) {
  const name = entrypoint === '.' ? 'root' : entrypoint.slice(2).replaceAll('/', '-')
  return path.join(root, 'etc/api', `${name}.api.md`)
}

function moduleExports(sourcePaths) {
  const config = ts.readConfigFile(path.join(root, 'tsconfig.build.json'), ts.sys.readFile)
  if (config.error !== undefined) {
    throw new Error(ts.flattenDiagnosticMessageText(config.error.messageText, '\n'))
  }
  const parsed = ts.parseJsonConfigFileContent(config.config, ts.sys, root)
  const program = ts.createProgram({
    rootNames: sourcePaths.map(sourcePath => path.join(root, sourcePath)),
    options: parsed.options
  })
  const checker = program.getTypeChecker()
  const result = new Map()
  for (const sourcePath of sourcePaths) {
    const absolutePath = path.join(root, sourcePath)
    const sourceFile = program.getSourceFile(absolutePath)
    if (sourceFile === undefined) throw new Error(`API report source is missing: ${sourcePath}`)
    const symbol = checker.getSymbolAtLocation(sourceFile)
    if (symbol === undefined) throw new Error(`API report module has no symbol: ${sourcePath}`)
    result.set(sourcePath, Object.freeze(checker.getExportsOfModule(symbol).map(exported => exported.getName()).sort()))
  }
  return result
}

function signatureForSymbol(checker, exported) {
  const flags = ts.TypeFormatFlags.NoTruncation | ts.TypeFormatFlags.WriteArrowStyleSignature
  const type =
    exported.flags & ts.SymbolFlags.Interface || exported.flags & ts.SymbolFlags.TypeAlias
      ? checker.getDeclaredTypeOfSymbol(exported)
      : checker.getTypeOfSymbol(exported)
  return checker.typeToString(type, undefined, flags).replace(/\s+/g, ' ').replaceAll('`', "'")
}

function collectExportEntries(sourcePaths) {
  const config = ts.readConfigFile(path.join(root, 'tsconfig.build.json'), ts.sys.readFile)
  if (config.error !== undefined) {
    throw new Error(ts.flattenDiagnosticMessageText(config.error.messageText, '\n'))
  }
  const parsed = ts.parseJsonConfigFileContent(config.config, ts.sys, root)
  const program = ts.createProgram({
    rootNames: sourcePaths.map(sourcePath => path.join(root, sourcePath)),
    options: parsed.options
  })
  const checker = program.getTypeChecker()
  const result = new Map()
  for (const sourcePath of sourcePaths) {
    const absolutePath = path.join(root, sourcePath)
    const sourceFile = program.getSourceFile(absolutePath)
    if (sourceFile === undefined) throw new Error(`API report source is missing: ${sourcePath}`)
    const symbol = checker.getSymbolAtLocation(sourceFile)
    if (symbol === undefined) throw new Error(`API report module has no symbol: ${sourcePath}`)
    const entries = checker
      .getExportsOfModule(symbol)
      .map(exported =>
        Object.freeze({
          name: exported.getName(),
          signature: signatureForSymbol(checker, exported)
        })
      )
      .sort((left, right) =>
        left.name < right.name ? -1 : left.name > right.name ? 1 : left.signature < right.signature ? -1 : 1
      )
    result.set(sourcePath, Object.freeze(entries))
  }
  return result
}

function verifiedSection(entrypoint, sourcePath, exports) {
  const lines = Array.isArray(exports) && typeof exports[0] === 'object' && exports[0] !== null
    ? exports.map(entry => `- \`${entry.name} :: ${entry.signature}\``)
    : exports.map(name => `- \`${name}\``)
  return [
    reportMarker.trimEnd(),
    '<!-- This section is generated by scripts/docs/check-api-reports.js. -->',
    `<!-- entrypoint: ${entrypoint}; source: ${sourcePath} -->`,
    '',
    ...lines,
    ''
  ].join('\n')
}

function updateReport(absolutePath, entrypoint, sourcePath, exports) {
  const current = fs.existsSync(absolutePath) ? fs.readFileSync(absolutePath, 'utf8') : `# API Report — ${entrypoint}\n`
  const prefix = current.split(reportMarker)[0].trimEnd()
  fs.writeFileSync(absolutePath, `${prefix}${verifiedSection(entrypoint, sourcePath, exports)}`)
}

function parseVerifiedSection(document, entrypoint, sourcePath, expectedExports) {
  const normalizedDocument = document.replace(/\r\n/g, '\n')
  const markerIndex = normalizedDocument.indexOf(reportMarker)
  if (markerIndex === -1) {
    throw new Error('missing generated verified-symbols section marker')
  }

  const lines = normalizedDocument.slice(markerIndex + reportMarker.length).split('\n')
  const expectedGeneratorComment = '<!-- This section is generated by scripts/docs/check-api-reports.js. -->'
  const expectedEntrypointComment = `<!-- entrypoint: ${entrypoint}; source: ${sourcePath} -->`
  if (lines[0] !== expectedGeneratorComment || lines[1] !== expectedEntrypointComment || lines[2] !== '') {
    throw new Error('malformed generated verified-symbols section header')
  }
  if (lines.at(-1) !== '') {
    throw new Error('malformed generated verified-symbols section terminator')
  }

  const symbols = []
  const signatures = new Map()
  const seen = new Set()
  for (let index = 3; index < lines.length - 1; index += 1) {
    const line = lines[index]
    const match = /^- `([^`\r\n]+)`$/.exec(line)
    if (match === null) {
      throw new Error(`malformed generated entry at line ${index + 1}: ${line}`)
    }
    const inner = match[1]
    const separator = ' :: '
    const separatorIndex = inner.indexOf(separator)
    const symbol = separatorIndex === -1 ? inner : inner.slice(0, separatorIndex)
    if (seen.has(symbol)) {
      throw new Error(`duplicate generated symbol: ${symbol}`)
    }
    seen.add(symbol)
    symbols.push(symbol)
    if (separatorIndex !== -1) signatures.set(symbol, inner.slice(separatorIndex + separator.length))
  }

  const expectedNames = expectedExports.map(entry => (typeof entry === 'string' ? entry : entry.name))
  const stale = symbols.filter(symbol => !expectedNames.includes(symbol))
  if (stale.length > 0) {
    throw new Error(`stale exported symbols: ${stale.join(', ')}`)
  }
  const missing = expectedNames.filter(symbol => !seen.has(symbol))
  if (missing.length > 0) {
    throw new Error(`missing exported symbols: ${missing.join(', ')}`)
  }
  if (expectedExports.length > 0 && typeof expectedExports[0] !== 'string') {
    for (const entry of expectedExports) {
      const actual = signatures.get(entry.name)
      if (actual !== entry.signature) {
        throw new Error(`signature mismatch: ${entry.name}`)
      }
    }
  }
  return symbols
}

function checkReports() {
  const sources = [...new Set(Object.values(typeScriptEntrypoints))]
  const exportsBySource = collectExportEntries(sources)
  const failures = []
  for (const [entrypoint, sourcePath] of Object.entries(typeScriptEntrypoints)) {
    const absolutePath = reportPath(entrypoint)
    const exports = exportsBySource.get(sourcePath)
    if (exports === undefined) throw new Error(`No export set was resolved for ${sourcePath}`)
    if (writeReports) {
      updateReport(absolutePath, entrypoint, sourcePath, exports)
      continue
    }
    if (!fs.existsSync(absolutePath)) {
      failures.push(`${path.relative(root, absolutePath)} is missing for package export ${entrypoint}`)
      continue
    }
    const document = fs.readFileSync(absolutePath, 'utf8')
    try {
      parseVerifiedSection(document, entrypoint, sourcePath, exports)
    } catch (error) {
      failures.push(`${path.relative(root, absolutePath)}: ${error.message}`)
    }
  }

  if (failures.length > 0) {
    throw new Error(`API report coverage failed:\n${failures.map(failure => `- ${failure}`).join('\n')}`)
  }

  if (writeReports) {
    console.log(
      `API reports written: ${Object.keys(typeScriptEntrypoints).length} TypeScript entrypoints; excluded metadata/config: ${nonTypeScriptEntrypoints.join(', ')}`
    )
  } else {
    console.log(
      `API reports checked: ${Object.keys(typeScriptEntrypoints).length} TypeScript entrypoints; excluded metadata/config: ${nonTypeScriptEntrypoints.join(', ')}`
    )
  }
}

if (require.main === module) checkReports()

module.exports = { parseVerifiedSection, collectExportEntries, moduleExports }
