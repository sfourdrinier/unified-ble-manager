const fs = require('node:fs')
const path = require('node:path')
const YAML = require('yaml')
const os = require('node:os')
const { spawnSync } = require('node:child_process')

const root = path.resolve(__dirname, '..')
const productionText = fs.readFileSync(path.join(root, '.github/workflows/publish.yml'), 'utf8')
const production = YAML.parse(productionText)
const { buildDraft, renderDraft, PUBLICATION_ONLY_STEPS } = require('../scripts/ci/generate-parallel-publisher-draft')

test('draft is manual, read-only, and has no publishing authority', () => {
  const draft = buildDraft(production)
  expect(Object.keys(draft.on)).toEqual(['workflow_dispatch'])
  expect(draft.permissions).toEqual({ contents: 'read' })
  for (const job of Object.values(draft.jobs)) {
    expect(job.environment).toBeUndefined()
    expect(job.permissions).toBeUndefined()
    for (const step of job.steps) {
      expect(step.run || '').not.toMatch(/npm publish|gh release create|git push/)
      expect(step['continue-on-error']).toBeUndefined()
    }
  }
})

test('every production prepublication step is retained, with commands unchanged', () => {
  const draft = buildDraft(production)
  const steps = Object.values(draft.jobs).flatMap(job => job.steps)
  for (const original of production.jobs.publish.steps) {
    if (PUBLICATION_ONLY_STEPS.includes(original.name)) continue
    const counterpart = steps.find(step => step.name === original.name)
    expect(counterpart).toBeDefined()
    expect(counterpart.run).toBe(original.run)
    expect(counterpart.uses).toBe(original.uses)
  }
})

test('native producers remain exact copies and source Android lanes start independently', () => {
  const draft = buildDraft(production)
  for (const name of ['native-prebuild-plan', 'native-prebuild', 'native-rustcore']) {
    const original = production.jobs[name]
    const copied = draft.jobs[name]
    for (const step of original.steps) expect(copied.steps).toContainEqual(step)
    expect(copied.strategy).toEqual(original.strategy)
    expect(copied['runs-on']).toBe(original['runs-on'])
  }
  expect(draft.jobs['android-classic'].needs).toBeUndefined()
  expect(draft.jobs['android-expo'].needs).toBeUndefined()
  for (const job of ['source-gates', 'packed-tauri', 'packed-smoke', 'packed-hosts', 'packed-g6a', 'packed-tv', 'packed-desktop']) {
    expect(draft.jobs[job].needs).toEqual(['canonical-package'])
    expect(draft.jobs[job].env.UBM_PACKED_TARBALL).toBe('${{ github.workspace }}/.release-package/canonical.tgz')
    expect(draft.jobs[job].steps.some(step => step.name === 'Verify immutable candidate digest')).toBe(true)
  }
})

test('aggregate runs even after failure and requires every lane to succeed', () => {
  const draft = buildDraft(production)
  const aggregate = draft.jobs.results
  expect(aggregate.if).toBe('${{ always() }}')
  expect(aggregate.needs.slice().sort()).toEqual(Object.keys(draft.jobs).filter(name => name !== 'results').sort())
  expect(aggregate.steps[0].run).toContain("job.result !== 'success'")
  expect(aggregate.steps[0].run).toContain('process.exitCode = 1')
})

test.each(['success', 'failure', 'cancelled', 'skipped'])('aggregate actually refuses non-success result %s', result => {
  const draft = buildDraft(production)
  const summary = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-draft-aggregate-test-'))
  try {
    const needs = Object.fromEntries(draft.jobs.results.needs.map(name => [name, { result: 'success', outputs: {} }]))
    needs['canonical-package'].outputs.sha256 = 'a'.repeat(64)
    needs['packed-tv'].result = result
    const run = spawnSync('bash', ['-e', '-o', 'pipefail', '-c', draft.jobs.results.steps[0].run], {
      encoding: 'utf8',
      env: { ...process.env, NEEDS_JSON: JSON.stringify(needs), SOURCE_SHA: 'b'.repeat(40), GITHUB_STEP_SUMMARY: path.join(summary, 'summary.md') }
    })
    expect(run.error).toBeUndefined()
    expect(run.status).toBe(result === 'success' ? 0 : 1)
    expect(fs.readFileSync(path.join(summary, 'summary.md'), 'utf8')).toContain(`packed-tv: ${result}`)
  } finally {
    fs.rmSync(summary, { recursive: true, force: true })
  }
})

test('candidate staging rejects another commit before unpacking', () => {
  const draft = buildDraft(production)
  const stage = draft.jobs['packed-smoke'].steps.find(item => item.name === 'Verify immutable candidate digest')
  const temporary = fs.mkdtempSync(path.join(os.tmpdir(), 'ubm-draft-source-test-'))
  try {
    fs.mkdirSync(path.join(temporary, '.release-package'))
    fs.writeFileSync(path.join(temporary, '.release-package/source-commit.txt'), 'wrong-commit\n')
    const run = spawnSync('bash', ['-e', '-o', 'pipefail', '-c', stage.run], {
      cwd: temporary, encoding: 'utf8',
      env: { ...process.env, GITHUB_SHA: 'b'.repeat(40) }
    })
    expect(run.error).toBeUndefined()
    expect(run.status).toBe(1)
    expect(run.stderr).not.toContain('tar:')
  } finally {
    fs.rmSync(temporary, { recursive: true, force: true })
  }
})

test('committed draft is generated without modifying production', () => {
  expect(fs.readFileSync(path.join(root, '.github/workflows/publish-parallel-draft.yml'), 'utf8')).toBe(renderDraft(production))
  expect(fs.readFileSync(path.join(root, '.github/workflows/publish.yml'), 'utf8')).toBe(productionText)
})

test('caches are optional, lane-isolated downloads, not shipping artifacts', () => {
  const draft = buildDraft(production)
  expect(draft.on.workflow_dispatch.inputs.cold_cache.default).toBe(true)
  for (const job of Object.values(draft.jobs)) {
    for (const step of job.steps.filter(item => item.uses === 'actions/cache@v4')) {
      expect(step.if).toBe('${{ !inputs.cold_cache }}')
      expect(step.with.key).toContain('${{ github.job }}')
      expect(step.with.path).not.toMatch(/target|prebuilds|jniLibs|RustCore|\.npmrc/)
      expect(step.with.path).toContain('~/.gradle/caches/modules-2')
    }
  }
})
