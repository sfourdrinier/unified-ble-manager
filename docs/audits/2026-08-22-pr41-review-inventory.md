# PR41 review inventory

The original round below is frozen against PR41 head
`b3a56e86632ba77c900bf3bbf4e812ce0da41204`. Every later item is separately
verified against the current source before disposition; review scope is never
silently expanded inside the original round.

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

## Follow-up inventory — current PR41 source

Verified against current HEAD `526a985c825164006458f663aa8fa086b26b4824`.
The current package gate is 139 suites / 1,271 tests; plugin is 36/36.

| ID | Source/reviewer | Finding | Disposition |
| --- | --- | --- | --- |
| F-1 | Hosted CI / current source | Packed third-party validator omitted the two PR8 base facts and rejected the packed artifact. | Confirmed and fixed in `077d797`; profile and regression fixture now require 43 facts. |
| F-2 | Luna adversarial review | G6A was present in `verify-release.sh` but absent from ordinary CI and publish. | Confirmed and fixed in `99ea289`; separate Linux Node 22 CI and Ubuntu Node 24 publish steps run the proof after generic pack smoke. |
| F-3 | Luna adversarial review | Web example and packed Web fixture passed public GATT objects to advanced internal helpers, used invalid stream options, and supplied `localNamePrefix: null`. | Confirmed and fixed in `212ea14`; public characteristic operations, public stream/delivery options, and valid chooser input are covered by regression tests. |
| F-4 | Luna xhigh adversarial review | A saturated native readiness ingress could drop a ready edge and leave a no-deadline write waiting forever. | Confirmed and fixed in `9a79842`; one bounded authoritative re-probe runs while false and is cancelled on all watch lifecycle paths. |
| F-5 | Luna xhigh adversarial review | Android API 24–25 advertised PHY despite native API-26 guards. | Confirmed and fixed in `6189f85`; optional handshake capability is fail-closed and runtime feature registration refreshes after open. |
| F-6 | Luna xhigh adversarial review | PR8 TCK PHY/readiness facts were descriptor-only; parameters/subrate were tautological. | Confirmed and fixed in `663cbc3`; PHY/readiness execute typed probes, parameters/subrate are explicitly scoped to their absent seam, and unsupported states cannot pass as supported. |
| F-7 | Luna adversarial review | Android source guard assumed only the monorepo layout. | Confirmed and fixed in `4972cfc`; installed `node_modules/unified-ble-manager/android` resolution is tested without broad traversal. |
| F-8 | Luna adversarial review | Normal pack/install npm subprocesses had no timeout. | Confirmed and fixed in `8d29232`; normal npm boundaries have a 600-second bound while G6A retains its 120-second child bound. |
| F-9 | Luna adversarial review | Advanced-only profile helpers were imported from the root in documentation. | Confirmed and fixed in `5269d28`; docs and consumer regression test use `/advanced`. |
| F-10 | Luna adversarial review | Unsupported security branches reported synthetic cancellation outcomes. | Confirmed and fixed in `663cbc3`; skipped outcomes are nullable “not observed” values while contract annotations remain explicit. |
| F-11 | Luna adversarial review | API report checker did not reject stale generated-symbol entries or malformed/duplicate verified sections. | Confirmed and fixed in `526a985c`; generated sections now fail closed on stale, malformed, duplicate, missing, or incomplete entries, with focused RED/GREEN tests. |

Remaining release gates at this checkpoint: push the final source, hosted CI including G6A and Android qualification, final exact-SHA adversarial review, the required external review rounds, merge PR41, then proceed to the plan’s RC3 checkpoint. RC2 remains immutable.
