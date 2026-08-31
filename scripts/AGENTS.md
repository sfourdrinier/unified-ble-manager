# AGENTS.md — scripts/

Conventions for the script tree. Root [`AGENTS.md`](../AGENTS.md) still
applies; this file adds only what is specific to `scripts/`.

- Generators own their outputs. `scripts/docs/*` own `docs/generated/*`,
  `docs/index.html`, and the `etc/api/*.api.md` reports; `scripts/evidence/*`
  and `scripts/release/*` own the evidence projections, SBOM, and license
  artifacts. Never hand-edit an output — change the source of truth, rerun the
  generator, and commit its output. Verify with `pnpm docs:check` and
  `pnpm release:artifacts:check`.
- Every generator has a `--check` (or dedicated check script) wired into a
  gate. A new generator is not done until its drift check is too.
- `scripts/ci/preflight.sh` runs the Linux-reproducible CI jobs against a
  clean detached worktree before a push; `--fast` skips the Android Gradle
  builds. Green means "worth pushing", never "CI will pass".
- Scripts are gated code: the test-first rule in the root `AGENTS.md` applies
  to them the same as to `src/`.
