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
    const tagGuard = workflow.indexOf('- name: Verify release tag points at current main')
    const bind = workflow.indexOf('- name: Bind npm tarball to the generated release artifact')
    const restore = workflow.indexOf('Restored exact published npm tarball for release recovery')

    expect(npmStatus).toBeGreaterThan(-1)
    expect(tagGuard).toBeGreaterThan(npmStatus)
    expect(workflow).toContain("if: steps.npm_status.outputs.package_published != 'true'")
    expect(bind).toBeGreaterThan(tagGuard)
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

  test('retries npm attestation download after publish instead of failing on a first 404', () => {
    const workflow = read('.github/workflows/publish.yml')
    const bindProvenance = workflow.indexOf('- name: Bind npm provenance to this exact tag commit')
    const download = workflow.indexOf('npm provenance attestations are not visible yet', bindProvenance)
    const verify = workflow.indexOf('verify-npm-provenance-source.js', bindProvenance)

    expect(bindProvenance).toBeGreaterThan(-1)
    expect(download).toBeGreaterThan(bindProvenance)
    expect(workflow).toContain('npm provenance attestations did not become visible within the bounded retry window')
    expect(verify).toBeGreaterThan(download)
  })

  test('cancels a superseded run of the same tag without cancelling a different version tag', () => {
    const workflow = read('.github/workflows/publish.yml')
    expect(workflow).toContain('group: ${{ github.workflow }}-${{ github.ref }}')
    expect(workflow).toMatch(/concurrency:\n(?:  .+\n)*  cancel-in-progress: true/)
  })

  test('the installed changelog points to preserved history that remains reachable outside the tarball', () => {
    const changelog = read('CHANGELOG.md')
    expect(changelog).toContain(
      'https://github.com/sfourdrinier/unified-ble-manager/blob/main/CHANGELOG_HISTORY.md'
    )
    expect(changelog).not.toContain('](CHANGELOG_HISTORY.md)')
  })
})
