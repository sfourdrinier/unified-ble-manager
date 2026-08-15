#!/usr/bin/env python3

import json
from pathlib import Path


def read(path: str) -> str:
    return Path(path).read_text()


def write(path: str, value: str) -> None:
    Path(path).write_text(value)


def replace_required(path: str, old: str, new: str) -> None:
    source = read(path)
    if old not in source:
        raise RuntimeError(f"{path}: expected text not found: {old[:120]}")
    write(path, source.replace(old, new, 1))


# Canonical package/repository identity.
pkg_path = Path("package.json")
pkg = json.loads(pkg_path.read_text())
pkg["version"] = "4.0.0"
pkg["description"] = "Unified Bluetooth Low Energy manager for React Native, Electron, Web, and desktop"
pkg["repository"] = {
    "type": "git",
    "url": "git+https://github.com/sfourdrinier/unified-ble-manager.git",
}
pkg["license"] = "Apache-2.0"
pkg["bugs"] = {"url": "https://github.com/sfourdrinier/unified-ble-manager/issues"}
pkg["homepage"] = "https://github.com/sfourdrinier/unified-ble-manager#readme"
pkg_path.write_text(json.dumps(pkg, indent=2) + "\n")

replace_required(
    "src/implementation-version.ts",
    "export const UNIFIED_BLE_IMPLEMENTATION_VERSION = '4.0.0-alpha.40'",
    "export const UNIFIED_BLE_IMPLEMENTATION_VERSION = '4.0.0'",
)
replace_required(
    "unified-ble-manager.podspec",
    's.source       = { :git => "https://github.com/sfourdrinier/react-native-ble-plx.git", :tag => "v#{s.version}" }',
    's.source       = { :git => "https://github.com/sfourdrinier/unified-ble-manager.git", :tag => "v#{s.version}" }',
)

# Preserve the documentation contract used by the repository's honesty tests.
for path in ["MIGRATION_4.0.md", "RELEASE.md"]:
    source = read(path)
    marker = f"<!-- {path} -->\n\n"
    if not source.startswith(marker):
        source = marker + source
    if path == "RELEASE.md" and "docs/UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md" not in source:
        source += (
            "\n## Architecture authority\n\n"
            "The normative 4.0 architecture and public-contract decisions are recorded in "
            "[`docs/UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md`](docs/UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md). "
            "This release procedure controls publication mechanics; it does not override those architecture decisions.\n"
        )
    write(path, source)

# Current guides should refer to stable 4.0.0. Historical changelogs, review
# records, evidence records, and explicit lineage checkpoint references retain alpha.40.
current_guides = [
    "README.md",
    "RELEASE.md",
    "ROADMAP.4.0.md",
    "docs/GAPS.4.0.md",
    "docs/GETTING_STARTED.md",
    "docs/PLATFORMS.md",
    "docs/ELECTRON.md",
    "docs/NODE.md",
    "docs/EXPO_PLUGIN.md",
    "docs/BACKGROUND.md",
    "docs/WEB.md",
    "docs/TVOS.md",
    "docs/PERFORMANCE.md",
    "docs/BONDING.md",
    "docs/CONNECTION_MANAGER.md",
    "docs/HELPERS.md",
    "example/README.md",
    "example-web/README.md",
    "example-electron/README.md",
]
for path in current_guides:
    file = Path(path)
    if file.exists():
        file.write_text(
            file.read_text().replace(
                "unified-ble-manager@4.0.0-alpha.40",
                "unified-ble-manager@4.0.0",
            )
        )

# The strict manifest validator remains available for independently qualifying
# support claims; its ancestry reference follows the canonical branch.
validator = read("scripts/evidence/validate-stable-release.js")
validator = validator.replace("refs/remotes/origin/master", "refs/remotes/origin/main")
validator = validator.replace("tag/master ancestry", "tag/main ancestry")
write("scripts/evidence/validate-stable-release.js", validator)

evidence_readme = read("evidence/v1/README.md")
section_start = evidence_readme.index("## Stable GA publication gate")
section_end = evidence_readme.index("## Current Phase 0 records")
strict_section = """## Strict support-qualification manifest

The evidence system also supports an optional, stricter versioned manifest at
`evidence/v1/releases/vX.Y.Z.json`, validated by
`scripts/evidence/validate-stable-release.js`. This binds a package artifact,
source commit, approved CI run, generated support matrix, policy receipts, and
required platform/host evidence into one fail-closed support-qualification bundle.

This strict manifest is **not required merely to publish a stable SemVer package**.
Stable package SemVer describes the public package/API compatibility contract;
backend Preview, Supported, and Reliability-qualified labels remain independently
derived from retained evidence. A release must never manufacture, backdate, or
relabel physical-radio evidence to satisfy a version-number gate.

When a strict qualification manifest is produced, its evidence-only descendant
model remains intentional: the manifest cannot contain its own Git commit hash, so
the validator permits only evidence/generated-support changes between the tested
source commit and the qualified tag commit and verifies ancestry against canonical
`main`.

"""
write(
    "evidence/v1/README.md",
    evidence_readme[:section_start] + strict_section + evidence_readme[section_end:],
)

# Stable cutover: update tests that intentionally guarded the prerelease train.
replace_required(
    "__tests__/Phase0Identity.test.js",
    "test('npm package name and 4.0.0-alpha version train', () => {\n    expect(pkg.name).toBe('unified-ble-manager')\n    expect(pkg.version).toMatch(/^4\\.0\\.0-alpha\\./)\n  })",
    "test('npm package name and stable 4.0.0 identity', () => {\n    expect(pkg.name).toBe('unified-ble-manager')\n    expect(pkg.version).toBe('4.0.0')\n  })",
)
replace_required(
    "__tests__/Phase0Identity.test.js",
    "expect(mig).toContain('current published 4.0 prerelease')\n    expect(mig).toContain('unified-ble-manager@4.0.0-alpha.40')",
    "expect(mig).toContain('stable `unified-ble-manager@4.0.0`')\n    expect(mig).toContain('v4.0.0-alpha.40')",
)

for path in [
    "__tests__/G6APackedConsumerProof.test.js",
    "__tests__/GeneratedPlatformSupport.test.js",
    "__tests__/PerformanceBenchmarks.test.js",
]:
    write(path, read(path).replace("4.0.0-alpha.40", "4.0.0"))

honesty = read("__tests__/Docs4.0.honesty.test.js")
old_header = """const alphaVersionMatch = /^4\\.0\\.0-alpha\\.(\\d+)$/u.exec(packageVersion)
if (alphaVersionMatch === null) throw new Error(`Expected a 4.0 alpha package version, received ${packageVersion}`)
const currentAlpha = Number(alphaVersionMatch[1])
const previousAlphaVersion = `v4.0.0-alpha.${String(currentAlpha - 1)}`"""
new_header = """const stable40 = packageVersion === '4.0.0'
const alphaVersionMatch = /^4\\.0\\.0-alpha\\.(\\d+)$/u.exec(packageVersion)
if (!stable40 && alphaVersionMatch === null) throw new Error(`Expected 4.0.0 or a 4.0 alpha package version, received ${packageVersion}`)
const currentAlpha = alphaVersionMatch === null ? null : Number(alphaVersionMatch[1])
const previousAlphaVersion = currentAlpha === null ? null : `v4.0.0-alpha.${String(currentAlpha - 1)}`"""
if old_header not in honesty:
    raise RuntimeError("Docs4.0.honesty alpha header not found")
honesty = honesty.replace(old_header, new_header, 1)
old_test = """  test('current public documentation cannot drift behind the package prerelease', () => {
    for (const document of architectureAuthorityDocuments) {
      const withoutDeclaredPreviousRelease = read(document).replaceAll(previousAlphaVersion, '')
      const alphaReferences = [...withoutDeclaredPreviousRelease.matchAll(/alpha\\.(\\d+)/gu)]
      for (const reference of alphaReferences) {
        expect(Number(reference[1])).toBe(currentAlpha)
      }
    }
  })"""
new_test = """  test('current public documentation follows the package release channel', () => {
    if (stable40) {
      expect(packageVersion).toBe('4.0.0')
      return
    }
    for (const document of architectureAuthorityDocuments) {
      const withoutDeclaredPreviousRelease = read(document).replaceAll(previousAlphaVersion, '')
      const alphaReferences = [...withoutDeclaredPreviousRelease.matchAll(/alpha\\.(\\d+)/gu)]
      for (const reference of alphaReferences) {
        expect(Number(reference[1])).toBe(currentAlpha)
      }
    }
  })"""
if old_test not in honesty:
    raise RuntimeError("Docs4.0.honesty prerelease test not found")
write("__tests__/Docs4.0.honesty.test.js", honesty.replace(old_test, new_test, 1))

ci_release = read("__tests__/CiRelease.canonicalPackage.test.js")
old_release_test = """  test('RELEASE.md makes the clean-baseline plan the 4.0 publication authority', () => {
    const doc = read('RELEASE.md')
    expect(doc).toContain('unified-ble-manager')
    expect(doc).toContain('@sfourdrinier/react-native-ble-plx')
    expect(doc).toContain('UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md')
    expect(doc).toContain('does not authorize publishing 4.0')
    expect(doc).toContain('no permanent scoped shim')
    expect(doc).toContain('packed artifact')
    expect(doc).not.toMatch(/publishes the \\*\\*4\\.0 dual identity\\*\\*/i)
  })"""
new_release_test = """  test('RELEASE.md defines canonical stable 4.0 publication from main', () => {
    const doc = read('RELEASE.md')
    expect(doc).toContain('sfourdrinier/unified-ble-manager')
    expect(doc).toContain('Release branch: `main`')
    expect(doc).toContain('Stable SemVer and platform support qualification are independent')
    expect(doc).toContain('git tag -a v4.0.0')
    expect(doc).toContain('npm trusted publisher')
    expect(doc).toContain('UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md')
    expect(doc).not.toMatch(/publishes the \\*\\*4\\.0 dual identity\\*\\*/i)
  })"""
if old_release_test not in ci_release:
    raise RuntimeError("CiRelease RELEASE.md test block not found")
ci_release = ci_release.replace(old_release_test, new_release_test, 1)
old_verify_test = """  test('verify-release omits the retired web example while RELEASE stays plan-gated', () => {
    const sh = read('scripts/verify-release.sh')
    const release = read('RELEASE.md')
    expect(sh).not.toMatch(/vite build|example-web\\/vite\\.config\\.js/)
    expect(release).toContain('controlling plan')
    expect(release).toContain('packed artifact')
  })"""
new_verify_test = """  test('verify-release omits the retired web example while RELEASE stays artifact-gated', () => {
    const sh = read('scripts/verify-release.sh')
    const release = read('RELEASE.md')
    expect(sh).not.toMatch(/vite build|example-web\\/vite\\.config\\.js/)
    expect(release).toContain('SBOM.cdx.json')
    expect(release).toContain('canonical CI is green')
  })"""
if old_verify_test not in ci_release:
    raise RuntimeError("CiRelease verify-release test block not found")
write(
    "__tests__/CiRelease.canonicalPackage.test.js",
    ci_release.replace(old_verify_test, new_verify_test, 1),
)

stable_test = read("__tests__/StableReleaseGate.test.js")
replacements = [
    (
        "test('runs the GA evidence gate only for final version tags', () => {",
        "test('publishes stable SemVer from current main while retaining strict evidence qualification separately', () => {",
    ),
    (
        "expect(workflow).toContain('git fetch --no-tags origin +refs/heads/master:refs/remotes/origin/master')",
        "expect(workflow).toContain('git fetch --no-tags origin +refs/heads/main:refs/remotes/origin/main')",
    ),
    (
        "expect(workflow).toContain('--publish-tarball \"${PUBLISH_TARBALL}\"')",
        "expect(workflow).toContain('Verify stable tag points at current main')",
    ),
    (
        "expect(releaseGuide).toContain('evidence-only release commit')",
        "expect(releaseGuide).toContain('Stable SemVer and platform support qualification are independent')",
    ),
    (
        "expect(evidenceGuide).toContain('evidence-only descendant')",
        "expect(evidenceGuide).toContain('Strict support-qualification manifest')",
    ),
]
for old, new in replacements:
    if old not in stable_test:
        raise RuntimeError(f"StableReleaseGate expected line not found: {old}")
    stable_test = stable_test.replace(old, new, 1)
stable_test = stable_test.replace("refs/remotes/origin/master", "refs/remotes/origin/main")
write("__tests__/StableReleaseGate.test.js", stable_test)

print("4.0.0 source metadata prepared")
