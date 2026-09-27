const fs = require('fs')
const path = require('path')

const root = path.resolve(__dirname, '..')
const read = file => fs.readFileSync(path.join(root, file), 'utf8')

function assertWarmupOrder(preflight) {
  const warmup = preflight.indexOf('if ! cargo fetch --locked')
  const install = preflight.indexOf('pnpm install --frozen-lockfile')
  const toolchain = preflight.indexOf('export PATH="$PINNED_TOOLCHAIN_BIN:$PATH"')
  expect(install).toBeGreaterThanOrEqual(0)
  expect(toolchain).toBeGreaterThanOrEqual(0)
  expect(warmup).toBeGreaterThan(install)
  expect(warmup).toBeGreaterThan(toolchain)
  expect(warmup).toBeLessThan(preflight.indexOf('run_package()'))
  expect(warmup).toBeLessThan(preflight.indexOf('run_tauri()'))
}

describe('clean-runner Cargo prerequisites', () => {
  test('preflight matches the hosted root-workspace offline metadata prerequisite', () => {
    const preflight = read('scripts/ci/preflight.sh')
    expect(preflight).toContain('if ! cargo fetch --locked > "$CACHE/cargo-fetch.log" 2>&1; then')
    assertWarmupOrder(preflight)
    for (const file of ['.github/workflows/ci.yml', '.github/workflows/publish.yml']) {
      expect(read(file)).toContain('run: cargo fetch --locked')
    }
  })

  test.each(['pnpm install --frozen-lockfile', 'export PATH="$PINNED_TOOLCHAIN_BIN:$PATH"'])(
    'the ordering guard rejects a missing prerequisite: %s',
    prerequisite => {
      const without = read('scripts/ci/preflight.sh').replace(prerequisite, 'removed prerequisite')
      expect(() => assertWarmupOrder(without)).toThrow()
    }
  )

  test('a failed warmup stops before offline gates instead of depending on a racing build cache', () => {
    const preflight = read('scripts/ci/preflight.sh')
    expect(preflight).toMatch(
      /if ! cargo fetch --locked[^\n]*; then\n[\s\S]*?tail -20 "\$CACHE\/cargo-fetch\.log"; echo "cargo fetch failed"; exit 1\nfi/
    )
    // No target filter: root metadata resolves dependencies for every target,
    // including wasm's js-sys even when preflight itself runs on Linux.
    const fetchLine = preflight.split('\n').find(line => line.startsWith('if ! cargo fetch'))
    expect(fetchLine).not.toContain('--target')
    expect(fetchLine).not.toContain('--offline')
    expect(read('scripts/release/generate-dependency-artifacts.js')).toContain(
      "['metadata', '--locked', '--offline', '--format-version', '1']"
    )
  })
})
