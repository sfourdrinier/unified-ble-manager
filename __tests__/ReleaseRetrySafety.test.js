'use strict'

const fs = require('fs')
const path = require('path')

const root = path.resolve(__dirname, '..')
const read = relativePath =>
  fs.readFileSync(path.join(root, relativePath), 'utf8').replace(/\r\n/g, '\n')

describe('release retry safety', () => {
  test('published reruns bypass current-main publication admission and reuse immutable npm bytes', () => {
    const workflow = read('.github/workflows/publish.yml')
    const npmStatus = workflow.indexOf('- name: Check whether versions are already on npm')
    const stableGuard = workflow.indexOf('- name: Verify stable tag points at current main')
    const bind = workflow.indexOf('- name: Bind npm tarball to the generated release artifact')
    const restore = workflow.indexOf('Restored exact published npm tarball for release recovery')

    expect(npmStatus).toBeGreaterThan(-1)
    expect(stableGuard).toBeGreaterThan(npmStatus)
    expect(workflow).toContain(
      "if: steps.release_channel.outputs.is_stable == 'true' && steps.npm_status.outputs.package_published != 'true'"
    )
    expect(bind).toBeGreaterThan(stableGuard)
    expect(restore).toBeGreaterThan(bind)
    expect(workflow).toContain(
      'ALREADY_PUBLISHED: ${{ steps.npm_status.outputs.package_published }}'
    )
    expect(workflow).toContain(
      'cp "${REGISTRY_TARBALL_COPY}" "${PUBLISH_TARBALL}"'
    )
    expect(workflow).toContain(
      '--output "${REGISTRY_TARBALL_COPY}"'
    )
    expect(workflow.indexOf('LOCAL_TARBALL_SHA256=')).toBeGreaterThan(restore)
  })

  test('the installed changelog points to preserved history that remains reachable outside the tarball', () => {
    const changelog = read('CHANGELOG.md')
    expect(changelog).toContain(
      'https://github.com/sfourdrinier/unified-ble-manager/blob/main/CHANGELOG_HISTORY.md'
    )
    expect(changelog).not.toContain('](CHANGELOG_HISTORY.md)')
  })
})
