const fs = require('fs')
const path = require('path')

const root = path.resolve(__dirname, '..')
const read = file => fs.readFileSync(path.join(root, file), 'utf8')

describe('clean-runner Cargo prerequisites', () => {
  test('preflight matches the hosted root-workspace offline metadata prerequisite', () => {
    const preflight = read('scripts/ci/preflight.sh')
    expect(preflight).toContain('if ! cargo fetch --locked > "$CACHE/cargo-fetch.log" 2>&1; then')
    const warmup = preflight.indexOf('if ! cargo fetch --locked')
    expect(warmup).toBeGreaterThan(preflight.indexOf('pnpm install --frozen-lockfile'))
    expect(warmup).toBeGreaterThan(preflight.indexOf('export PATH="$PINNED_TOOLCHAIN_BIN:$PATH"'))
    expect(warmup).toBeLessThan(preflight.indexOf('run_package()'))
    expect(warmup).toBeLessThan(preflight.indexOf('run_tauri()'))
    for (const file of ['.github/workflows/ci.yml', '.github/workflows/publish.yml']) {
      expect(read(file)).toContain('run: cargo fetch --locked')
    }
  })

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
