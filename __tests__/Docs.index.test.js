// __tests__/Docs.index.test.js

const { checkDocsIndex } = require('../scripts/docs/check-docs-index')
const fs = require('node:fs')
const path = require('node:path')
const read = file => fs.readFileSync(path.join(__dirname, '..', file), 'utf8')

describe('documentation index and agent-facing docs consistency', () => {
  test('the documentation index and llms.txt cover every document and entrypoint', () => {
    expect(checkDocsIndex()).toEqual([])
  })

  test('current UBM migration is distinct from the non-copyable ble-plx history', () => {
    const migration = read('MIGRATION_4.0.28.md')
    const version = JSON.parse(read('package.json')).version
    expect(migration).toContain(`unified-ble-manager@${version}`)
    expect(JSON.parse(read('package.json')).files).toContain('MIGRATION_4.0.28.md')
    expect(read('scripts/ci/verify-package-tarballs.js')).toContain("'package/MIGRATION_4.0.28.md'")
    for (const fact of [
      'UnifiedBleRustCore',
      'restorationId',
      'generation',
      'manager.capabilities.supports',
      'manager.background.acquire',
      'delivery',
      'connection.failed',
      'connection.lost',
      'adapter-loss',
      'caller-decides',
      'androidGattStatus',
      'IPC protocol is 5',
      'C-UBM.0.1.2-DRAFT',
      'canonicalUuid'
    ]) {
      expect(migration).toContain(fact)
    }
    for (const front of ['README.md', 'llms.txt', 'docs/README.md']) {
      expect(read(front)).toContain('MIGRATION_4.0.28.md')
    }
    expect(read('MIGRATION_4.0.md')).toContain('Historical, non-copyable')
    expect(read('MIGRATION_4.0.md')).not.toContain('pnpm add unified-ble-manager\n')
    expect(read('MIGRATION_4.0.md')).not.toContain('manager.supports(id)')
  })

  test('expert and TV entrypoints remain visible to consumer agents', () => {
    for (const front of ['README.md', 'llms.txt']) {
      expect(read(front)).toContain('unified-ble-manager/advanced')
    }
    expect(read('llms.txt')).toContain('/docs/TV.md')
    expect(read('llms.txt')).toContain('/docs/BACKGROUND.md')
    expect(read('AGENTS.md')).toContain('[`llms.txt`](llms.txt)')
  })

  test('Tauri API report has one generated authority, not a stale handwritten signature', () => {
    const report = read('etc/api/tauri.api.md')
    const prefix = report.split('## Verified exported symbols')[0]
    expect(prefix).not.toContain('```ts')
    expect(report).toContain('ipcProtocol: 5')
    expect(report).not.toContain('ipcProtocol: 2')
  })
})
