# Recording group commit qualification — 2026-10-10

Evidence level: Linux native synthetic integration with FakeRadio/Scripted
boundaries and a real SQLite journal. This does not establish physical-radio,
hosted Windows, macOS or Apple device qualification.

Already-held values now share bounded synced transactions of at most 32 records,
without waiting for a full batch. Normal mobile/desktop session collection yields
between value groups and rotates fairly across routes. The queue admission result
atomically distinguishes overflow from an already-counted sealed cutoff; callers
do not infer the cause by inspecting seal state afterwards. Disposal-tail loss
after an existing terminal remains an explicit notification ingress-drop count.

The parent ran `cargo test --offline -j 4 -p ubm-desktop -p ubm-mobile` against the
integrated sources: exit 0; 78 suite result lines, 905 passed, 0 failed and 69
pre-existing ignored tests. `cargo fmt --all -- --check` and affected all-target
Clippy with `-D warnings` also exited 0. The final subsequent source edit only
clarifies the collection comment: terminal controls retain separate commits.

Meaningful regressions cover ordered row/ordinal/content equivalence, atomic
accepted prefixes at capacity, whole-group rollback on storage failure, fair
second-consumer progress, setup response behind a recording backlog, seal-after-
refusal classification, and disposal-tail loss without a second stream terminal.
Mutation runs remove rotation/one-group limits or loss reporting and fail the
corresponding assertions. Calibrated commit-cost tests model transaction overhead;
they are not hardware fsync measurements.

The hosted Windows failure before the first setup write was separately reproduced
on Linux with a 200ms filesystem-sync shim. The old three-second startup wait
failed after 3.04s; diagnostic measurement placed the first write at 4.074s. The
corrected fixture permits the native connect/discovery startup allowance (35s).
With only the first 20 sync calls delayed, it passes in 4.48s while retaining all
original subsequent two-second setup and intake deadlines. No production deadline
was relaxed. Hosted Windows must rerun the frozen commit independently.

Parent logs: /tmp/ubm502-group-final-parent-tests.log,
/tmp/ubm502-group-final-parent-fmt.log, /tmp/ubm502-group-final-parent-clippy.log,
/tmp/ubm502-restored-startup-red.log and /tmp/ubm502-restored-startup-green.log.
The committed regressions preserve these contracts for CI; the local log paths
are session evidence, not distributable artifacts or CI receipts.

The subsequent combined candidate d360d5a9 passed the Mac desktop/mobile native
run: 78 result summaries, 903 reported passing tests, zero failed and three
hardware-only ignored tests. Hosted commit60e9234f separately failed the new
40ms commit-cost fixture because its measured cost was only28.36ms. Calibration
now measures the installed view twice, uses the faster result and makes at most
six bounded row-count adjustments, preserving the30ms minimum cost floor and
all original setup/backlog response deadlines. A real SQLite UPDATE regression
starts with one recursive row to require this adjustment path. A units mutation
fails that regression; the corrected complete continuation-adapter suite passes
27 tests. Native source identity remains unchanged by this test-only correction.

Clean Windows package validation also exposed an undeclared direct Babel preset;
the package now declares the matching React Native0.86 development preset.
Canonical dependency artifact generation updates only the lockfile hash in SBOM
and license inventory. The initial Linux package run reported four stale-artifact
failures before that refresh; those failures remain recorded. Fresh final-candidate
package, hosted and Windows gates are still required before publication.
