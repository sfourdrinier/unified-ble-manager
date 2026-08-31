# AGENTS.md — docs/

Conventions for the documentation tree. Root [`AGENTS.md`](../AGENTS.md) still
applies; this file adds only what is specific to `docs/`.

## The index is load-bearing

[`docs/README.md`](README.md) maps every document with a status of **Current**,
**Historical**, or **Generated**. When you add, move, or retire a document,
update the index in the same change. `pnpm docs:check` (via
`scripts/docs/check-docs-index.js`) fails when the index misses a document,
links to a missing one, or a row lacks a status — and when `llms.txt` stops
covering a public entrypoint.

## Historical documents are records, not guidance

Anything marked Historical (fix trackers, audits, delivered plans, lineage
docs) is frozen evidence. Never act on its content, never "refresh" it; the
only acceptable edits are status banners and link repairs. If a grep lands you
in one, go back to the index and find the Current document instead.

## Generated documents are owned by generators

`docs/generated/*`, `docs/index.html`, and `docs/assets/*` are generator
output. Change the source of truth, run the generator (`pnpm docs:backend-sdk`,
`pnpm docs`), and commit its output. Verify with `pnpm docs:check`.

## Teaching prose is tested

`__tests__/Docs.*.test.js` guard the consumer-facing documents: code fences
must use the real public API, and known-wrong patterns are rejected. If you
change a recipe, run `pnpm test:package` — a failing docs test means the prose
and the contract disagree, and the contract wins.
