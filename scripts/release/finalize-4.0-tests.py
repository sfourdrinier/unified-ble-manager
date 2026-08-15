#!/usr/bin/env python3

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


def replace_test(path: str, test_name: str, replacement: str) -> None:
    source = read(path)
    marker = f"  test('{test_name}'"
    start = source.find(marker)
    if start < 0:
        raise RuntimeError(f"{path}: test not found: {test_name}")
    candidates = [
        source.find("\n  test('", start + len(marker)),
        source.find("\n  test.each(", start + len(marker)),
    ]
    candidates = [position for position in candidates if position >= 0]
    end = min(candidates) if candidates else source.rfind("\n})")
    if end <= start:
        raise RuntimeError(f"{path}: could not find end of test: {test_name}")
    write(path, source[:start] + replacement.rstrip() + "\n" + source[end:])


# Stable package identity: the alpha-train guard becomes an exact stable-version guard.
replace_required(
    "__tests__/PackageModernization.js",
    "expect(rootPackage.version).toMatch(/^4\\.0\\.0-alpha\\./)",
    "expect(rootPackage.version).toBe('4.0.0')",
)

# Keep the public README connected to the normative architecture authority rather
# than making the stable landing page look like a separate source of truth.
readme = read("README.md")
authority_line = "**Architecture authority:** [`docs/UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md`](docs/UNIFIED_BLE_4.0_IMPLEMENTATION_PLAN.md)"
if authority_line not in readme:
    stable_intro = "**4.0.0 is the first stable release of the Unified BLE Manager package and public API contract.** It is a new package line, not a source-compatible rename of `react-native-ble-plx` 3.x."
    if stable_intro not in readme:
        raise RuntimeError("README stable introduction not found")
    readme = readme.replace(stable_intro, stable_intro + "\n\n" + authority_line, 1)
    write("README.md", readme)

# SECURITY.md now gives a safer fallback when GitHub private vulnerability reporting
# has not yet been enabled instead of forbidding the existence of a public issue at all.
replace_required(
    "__tests__/ReleaseArtifacts.test.js",
    "expect(security).toContain('Do not open a public issue')",
    "expect(security).toContain('Do not put vulnerability details in a public issue')",
)
replace_required(
    "__tests__/ReleaseArtifacts.test.js",
    "expect(support).toContain('Evidence-backed support labels')",
    "expect(support).toContain('Platform/backend support labels are a separate evidence-backed dimension')",
)

# The migration guide says exactly how Base64 remains available: explicit codecs at
# an external-protocol boundary, not a duplicated BLE method family.
replace_required(
    "__tests__/Phase0Identity.test.js",
    "expect(mig).toContain('Base64 only as an explicit codec helper')",
    "expect(mig).toContain('encode/decode explicitly through `unified-ble-manager/codecs`')",
)

# A stable package version such as 4.0.0 can legitimately equal other protocol/API
# version literals. Guard uniqueness of the implementation-version *declaration*,
# rather than uniqueness of the generic string literal across every source file.
version_parity = read("__tests__/PackageVersionParity.test.js")
old_version_block = """    const versionLiteral = `'${packageJson.version}'`
    const filesWithPackageVersionLiteral = sourceFiles(path.join(root, 'src'))
      .filter(file => fs.readFileSync(file, 'utf8').includes(versionLiteral))
      .map(file => path.relative(root, file).split(path.sep).join('/'))

    expect(filesWithPackageVersionLiteral).toEqual(['src/implementation-version.ts'])"""
new_version_block = """    const declaration = 'export const UNIFIED_BLE_IMPLEMENTATION_VERSION'
    const filesDeclaringImplementationVersion = sourceFiles(path.join(root, 'src'))
      .filter(file => fs.readFileSync(file, 'utf8').includes(declaration))
      .map(file => path.relative(root, file).split(path.sep).join('/'))

    expect(filesDeclaringImplementationVersion).toEqual(['src/implementation-version.ts'])"""
if old_version_block not in version_parity:
    raise RuntimeError("PackageVersionParity literal-uniqueness block not found")
write(
    "__tests__/PackageVersionParity.test.js",
    version_parity.replace(old_version_block, new_version_block, 1),
)

# Documentation self-audits must describe the stable public surface while retaining
# alpha.40 only as history/provenance.
docs_path = "__tests__/Docs4.0.honesty.test.js"

replace_test(
    docs_path,
    "gap inventory separates implemented code from missing physical proof",
    """  test('gap inventory separates implemented code from missing physical proof', () => {
    const gaps = read('docs/GAPS.4.0.md')

    expect(gaps).toContain('Current implementation and evidence inventory')
    expect(gaps).toContain('Implementation/package state')
    expect(gaps).toContain('Remaining evidence work')
    expect(gaps).toContain('Implemented contract/core/TCK path')
    expect(gaps).toContain('not architecture authority')
    expect(gaps).toContain('implementation proof')
    expect(gaps).not.toContain('WinRT remains incomplete')
    expect(gaps).not.toContain('The pre-4.0 source tree contains a transitional')
  })""",
)

replace_test(
    docs_path,
    "roadmap rejects compatibility, dual APIs, static matrices, Noble, and reduced scope",
    """  test('roadmap rejects compatibility, dual APIs, static matrices, Noble, and reduced scope', () => {
    const roadmap = read('ROADMAP.4.0.md')

    expect(roadmap).toMatch(/not a compatibility release/)
    expect(roadmap).toContain('bytes-only public/backend BLE contracts')
    expect(roadmap).toContain('does not preserve a permanent 3.x API')
    expect(roadmap).toContain('Meta Quest, peripheral mode, Bluetooth Classic, LE Audio, L2CAP CoC')
    expect(roadmap).toContain('remain deferred to 4.1')
    expect(roadmap).toContain('Stable `4.0.0` defines the public 4.x package/API contract')
    expect(roadmap).toContain('`v4.0.0-alpha.40` is retained as the final alpha')
    expect(roadmap).not.toMatch(/hard compatibility guarantee/i)
    expect(roadmap).not.toMatch(/Base64 still available unless 5\\.0/i)
    expect(roadmap).not.toMatch(/thin install\\/import shim/i)
  })""",
)

replace_test(
    docs_path,
    "migration documents the current v4 package without inventing a compatibility path",
    """  test('migration documents stable 4.0 without inventing a compatibility path', () => {
    const migration = read('MIGRATION_4.0.md')
    const release = read('RELEASE.md')

    expect(migration).toContain('stable `unified-ble-manager@4.0.0`')
    expect(migration).toContain('`hostSessionScope` should be a stable security/ownership scope')
    expect(migration).toContain('`Uint8Array`')
    expect(migration).toContain('`AbortSignal`')
    expect(migration).toContain('Await `manager.destroy()`')
    expect(migration).toContain('not a source-compatible rename')
    expect(migration).toContain('encode/decode explicitly through `unified-ble-manager/codecs`')
    expect(migration).toContain('`v4.0.0-alpha.40` is the repository-migration checkpoint')
    expect(migration).not.toMatch(/zero-change (JS )?API/i)
    expect(migration).not.toMatch(/optional bytes codemod/i)

    expect(release).toContain('Release branch: `main`')
    expect(release).toContain('Stable SemVer and platform support qualification are independent')
    expect(release).toContain('git tag -a v4.0.0')
    expect(release).not.toMatch(/publishes the \\*\\*4\\.0 dual identity\\*\\*/i)
  })""",
)

replace_test(
    docs_path,
    "public README provides only current alpha.40 prerelease construction and plugin guidance",
    """  test('public README provides stable 4.0 construction, plugin guidance, and preserved alpha history', () => {
    const readme = read('README.md')
    const changelog = read('CHANGELOG.md')
    const history = read('CHANGELOG_HISTORY.md')

    expect(readme).toContain('4.0.0 is the first stable release')
    expect(readme).toContain('pnpm add unified-ble-manager@4.0.0')
    expect(readme).toContain('`v4.0.0-alpha.40` was the migration point')
    expect(changelog).toContain('## [4.0.0] - 2026-08-16')
    expect(history).toContain('## [4.0.0-alpha.40]')
    expect(readme).toContain('createReactNativeBleManager')
    expect(readme).toContain('`hostSessionScope` is a stable host-owned security scope')
    expect(readme).toContain('`Uint8Array`')
    expect(readme).toContain('`AbortSignal`')
    expect(readme).toContain('iosNativeProtocolRestoration')
    expect(readme).not.toMatch(
      /iosEnableRestoration|iosRestorationIdentifier|iosNativeProtocolRestorationIdentifier|androidEnableForegroundService/
    )
    expect(readme).not.toMatch(/new\\s+BleManager\\s*\\(/)
  })""",
)

replace_test(
    docs_path,
    "alpha.40 published documentation preserves exact release, evidence, and deferral boundaries",
    """  test('stable 4.0 documentation preserves release, evidence, and deferral boundaries', () => {
    const readme = read('README.md')
    const release = read('RELEASE.md')
    const platforms = read('docs/PLATFORMS.md')

    expect(readme).toContain('Stable versions publish to npm `latest`; prereleases publish to `next`')
    expect(readme).toContain('npm trusted publishing/OIDC with provenance')
    expect(release).toContain('git tag -a v4.0.0')
    expect(release).toContain('npm trusted publishing/OIDC')
    expect(release).toContain('publishes with provenance')
    expect(platforms).toContain('`unified-ble-manager@4.0.0` is the first **stable package/API release**')
    expect(platforms).toContain('WinRT compilation or ABI loading, for example, is not by itself a Windows live-radio claim')
    expect(platforms).toContain('Meta Quest and the controllable nRF52840 fault-injection controller remain deferred to 4.1')
  })""",
)

replace_test(
    docs_path,
    "platform pages make instantiated backend evidence, not static source behavior, authoritative",
    """  test('platform pages make instantiated backend evidence, not static source behavior, authoritative', () => {
    const platforms = read('docs/PLATFORMS.md')
    const gaps = read('docs/GAPS.4.0.md')

    expect(platforms).toContain('not a static compatibility matrix')
    expect(platforms).toContain('typed capabilities of its instantiated backend')
    expect(gaps).toContain('not architecture authority')
    expect(gaps).toContain('implementation proof')
    expect(gaps).toContain('do not become physical-radio support evidence')
    expect(gaps).toContain('must never be presented as live-radio proof')
  })""",
)

print('stable 4.0 self-audits aligned')
