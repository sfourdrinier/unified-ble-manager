# PR41 review inventory

Frozen against PR41 head `b3a56e86632ba77c900bf3bbf4e812ce0da41204` after the
first Copilot and Codex review round. Every item is verified against current
source before disposition; later review comments are not silently added to
this round.

| ID | Reviewer | Finding | Disposition |
| --- | --- | --- | --- |
| C-1 | Copilot | TCK fairness probe settles `secondReadSettled` from the first queued read. | Confirmed and fixed: the second queued read owns the probe; focused TCK/queue tests pass. |
| C-2 | Copilot | `queueTraceIsBounded` is not included in the fairness fact. | Confirmed and fixed: quota trace evidence is required and overflow records a bounded trace entry. |
| C-3 | Copilot | TCK control-state helper does not exercise supported or unavailable states for several controls. | Confirmed and fixed: all four registered states are accepted; absent registration no longer silently passes. |
| C-4 | Copilot | Connection release returns before child/backend cleanup when readiness cleanup failures exist. | Confirmed and fixed: admission failures merge into the final receipt after teardown continues. |
| X-1 | Codex | Renderer IPC can expose a readiness capability that its renderer façade always rejects. | Confirmed and fixed: renderer-only control seams are projected unsupported until routed. |
| X-2 | Codex | Renderer `writeReadiness` maps a registered unavailable capability to unsupported. | Confirmed and fixed: unsupported and unavailable stream errors are distinct. |
| X-3 | Codex | `writeReadiness` TCK handling does not preserve supported/unavailable capability states. | Confirmed and fixed: capability-state truth is covered by TCK and IPC tests. |

Local evidence before this round: package 138 suites / 1248 tests, plugin
36/36, lint/typecheck/docs/API, native protocol, CoreBluetooth build, release
artifacts/evidence, performance, and diff checks green. Hosted CI and packed
consumer proof remain separate gates.
