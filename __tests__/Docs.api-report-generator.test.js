// __tests__/Docs.api-report-generator.test.js
const path = require('node:path')
const ts = require('typescript')

const { collectExportEntries, collectExportEntriesFromProgram } = require('../scripts/docs/check-api-reports')

const fixture = path.join(__dirname, 'helpers', 'api-reports', 'api-report-symbols.ts')
const noiseFixture = path.join(__dirname, 'helpers', 'api-reports', 'api-report-symbol-noise.ts')
const secondModuleFixture = path.join(__dirname, 'helpers', 'api-reports', 'api-report-symbols-second-module.ts')

function createFixtureProgram(rootNames) {
  return ts.createProgram(rootNames, {
    target: ts.ScriptTarget.ES2022,
    module: ts.ModuleKind.CommonJS,
    strict: true,
    lib: ['lib.es2020.d.ts']
  })
}

describe('API report generator signatures', () => {
  test('uses semantic names for well-known symbols in production entrypoints', () => {
    const entries = collectExportEntries(['src/index.ts', 'src/backend-sdk.ts'])
    const rootSignatures = entries
      .get('src/index.ts')
      .map(entry => entry.signature)
      .join('\n')
    const backendSignatures = entries
      .get('src/backend-sdk.ts')
      .map(entry => entry.signature)
      .join('\n')

    expect(rootSignatures).toContain('[Symbol.asyncIterator]')
    expect(rootSignatures).not.toMatch(/__@(?:asyncIterator|toStringTag|iterator)@[0-9]+/)
    expect(backendSignatures).toContain('[Symbol.toStringTag]')
    expect(backendSignatures).not.toMatch(/__@(?:asyncIterator|toStringTag|iterator)@[0-9]+/)
  })

  test('keeps distinct custom symbols and their types stable across compiler symbol IDs', () => {
    const withoutNoise = createFixtureProgram([fixture])
    const withNoise = createFixtureProgram([noiseFixture, fixture])
    const first = collectExportEntriesFromProgram(withoutNoise, [fixture]).get(fixture)
    const second = collectExportEntriesFromProgram(withNoise, [fixture]).get(fixture)

    expect(first).toEqual(second)
    const signature = first.find(entry => entry.name === 'SymbolSurface').signature
    expect(signature).toContain('[Symbol.asyncIterator]')
    expect(signature).toContain('[Symbol.toStringTag]: string')
    expect(signature).toContain(
      '[firstCustom /* __tests__/helpers/api-reports/api-report-symbols.ts#firstCustom */]: "first"'
    )
    expect(signature).toContain(
      '[secondCustom /* __tests__/helpers/api-reports/api-report-symbols.ts#secondCustom */]: "second"'
    )
    expect(signature).not.toMatch(/__@[A-Za-z]+@[0-9]+/)
  })

  test('distinguishes same-name unique symbols from different declaring modules', () => {
    const program = createFixtureProgram([fixture, secondModuleFixture])
    const entries = collectExportEntriesFromProgram(program, [fixture, secondModuleFixture])
    const first = entries.get(fixture).find(entry => entry.name === 'ReboundKeySurface').signature
    const second = entries.get(secondModuleFixture).find(entry => entry.name === 'ReboundKeySurface').signature

    expect(first).not.toBe(second)
    expect(first).toContain('rebound-first')
    expect(second).toContain('rebound-first')
    expect(first).toContain('firstUnique')
    expect(second).toContain('firstUnique')
    expect(first).toContain('api-report-symbols.ts#firstUnique')
    expect(second).toContain('api-report-symbols-second-module.ts#firstUnique')
  })

  test('detects alias target rebinding while expression and value stay identical', () => {
    const host = ts.createCompilerHost({ target: ts.ScriptTarget.ES2022, strict: true })
    const readFile = host.readFile.bind(host)
    host.readFile = name => {
      const content = readFile(name)
      return path.normalize(name) === path.normalize(fixture) && content !== undefined
        ? content.replaceAll('firstUnique', 'replacementUnique')
        : content
    }
    const original = collectExportEntriesFromProgram(createFixtureProgram([fixture]), [fixture]).get(fixture)
    const rebound = collectExportEntriesFromProgram(
      ts.createProgram([fixture], { target: ts.ScriptTarget.ES2022, strict: true }, host),
      [fixture]
    ).get(fixture)
    const originalSignature = original.find(entry => entry.name === 'ReboundKeySurface').signature
    const reboundSignature = rebound.find(entry => entry.name === 'ReboundKeySurface').signature
    expect(originalSignature).not.toBe(reboundSignature)
    expect(originalSignature).toContain('firstUnique')
    expect(reboundSignature).toContain('replacementUnique')
  })

  test('does not treat a shadowed Symbol object as a well-known symbol', () => {
    const source = path.join(__dirname, 'helpers', 'api-reports', 'api-report-symbols-shadow.ts')
    const entries = collectExportEntriesFromProgram(createFixtureProgram([source]), [source]).get(source)
    expect(entries.find(entry => entry.name === 'ShadowSurface').signature).toContain(
      'api-report-symbols-shadow.ts#custom'
    )
  })

  test('canonicalized backticks remain parseable in a verified section', () => {
    const program = createFixtureProgram([fixture])
    const entries = collectExportEntriesFromProgram(program, [fixture]).get(fixture)
    const report = [
      '',
      '## Verified exported symbols',
      '<!-- This section is generated by scripts/docs/check-api-reports.js. -->',
      `<!-- entrypoint: ./fixture; source: ${fixture} -->`,
      '',
      ...entries.map(entry => `- \`${entry.name} :: ${entry.signature}\``),
      ''
    ].join('\n')

    expect(() =>
      require('../scripts/docs/check-api-reports').parseVerifiedSection(report, './fixture', fixture, entries)
    ).not.toThrow()
  })
})
