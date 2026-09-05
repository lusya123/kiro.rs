# Q2 / POMO compatibility experiment

Date: 2026-09-05. Branch: `codex/q2-pomo-compat-20260905`.
Starting commit and live Q2 source: `68dd35c1e5c15bcf1e6328495f2858cc9e2f6c17`.

## User constraints

- Benchmark POMO first with the locally configured POMO credential.
- Test Opus 5, 4.8, 4.7, 4.6, Sonnet 5, Sonnet 4.6, and Haiku on all four requested sites.
- Maintain a complete 7 × 4 coverage matrix, explicitly recording unavailable models/site selectors; never substitute a different model.
- POMO protocol behavior has priority over detector scores. Preserve existing request formats, headers, body fields and parameters. Do not add system prompts. Leave thinking signature handling unchanged.
- Preserve general conversation, coding, tool calls, streaming, cache and error behavior; site-specific response fabrication is not an acceptable fix.
- Preserve the existing `msg_bdrk_` response ID prefix/behavior. User explicitly reaffirmed this while POMO's current `msg_` response IDs were being observed; do not imitate that difference or lengthen IDs for detector heuristics.
- Validate every code change locally before building a separate test image. Do not publish or overwrite AWS-B image tags or branches.
- Deploy only the standalone Q2 test instance at `q2.quietfox.sbs` on `43.156.115.199`; retain a rollback copy. Preserve other users' services.
- Save per-site scores, conditions, raw available requests/responses, and iteration receipts with secrets redacted.

## Execution stages

1. [done] Create isolated branch/worktree.
2. [done] Run unchanged local test suite and validate POMO credential/model catalog.
3. [done] Establish POMO website baseline and exact-probe direct response references.
4. [running] Extend Q2 baseline to all seven models and inspect specific differences.
5. [done] Select evidence-backed compatibility fixes within frozen protocol/signature constraints; add regression tests, then implement.
6. [done] Local full validation and unchanged request-shape/general coding checks.
7. [pending] Build immutable test-only image, deploy Q2 with rollback, repeat full matrix.
8. [pending] Compare scores and POMO parity; retain improvements only when compatibility and normal workload gates pass.

## Artifacts

- Previous 3-model complete capture: `outputs/q2-detailed-evidence-20260905/` in the main checkout (310 HTTP pairs, 28 complete SSE streams).
- New experiment artifacts are saved outside tracked code under the main checkout `outputs/q2-pomo-optimization-20260905/`.
- POMO credential source: current machine-local `~/.claude/settings.json` environment fields. Secret values are never copied into tracked files or reports.

## Operational boundary

The current Q2 is a standalone bridge-network test instance, not an AWS-B paired production member. Reconfirm the live identity/configuration before deployment. Production dual-cluster rollout procedures and gateway/database mutations are outside this task.

## Iteration 1: preserve ordinary structured response data

The unmodified Q2 context workload returned `service: Claude` after the user supplied `service = kiro`, on Sonnet 4.6 and Haiku. The fallback identity sanitizer split a fenced JSON answer into lines and mistook a business-data field for an assistant-name label. POMO references and all raw direct workload records are in the external experiment artifact directory.

The fix keeps objects/arrays and fenced code outside the ordinary-response fallback, retaining the existing strict identity path. Two regression tests failed before the fix and pass after it, covering bare JSON, fenced JSON/YAML and four stream chunk sizes. Request conversion, request headers, parameters, system prompts, response ID generation and thinking signature handling are unchanged.

Validation: frontend build completed; `cargo test --locked --no-default-features -- --test-threads=4` reports **976 passed, 0 failed, 3 ignored**. The unchanged baseline was 974 passed, 3 ignored. Formatting and `git diff --check` pass. A dedicated workflow publishes only `q2-test-<full commit SHA>`, with no production aliases.

POMO website baseline covers all 28 model/site cells: 20 supported runs and 8 explicitly unavailable selectors (Ranking and CheckHub). Failed connectivity attempts are retained separately. Current Q2 baseline and the deployed-image rerun are recorded as separate rounds; score changes must be measured rather than inferred from the fix.

## Iteration 2: stop rejecting supported explicit thinking

The complete Q2 capture contains 673 complete HTTP exchanges with no TCP gaps. API Dance's Sonnet 4.6 baseline was rejected locally with HTTP 400. Replaying the same captured enabled-thinking request against POMO returned HTTP 200 with thinking and signature for both Sonnet 4.6 and Opus 4.6.

The public-feature guard incorrectly reused the assistant-prefill capability to decide whether explicit thinking was supported, despite the existing model capability table allowing enabled thinking on both 4.6 models. The guard now consults that existing table before rejecting. It does not modify the request, request normalization, upstream headers, parameters, ID generation or signature code. Unsupported enabled-thinking protocols on newer models still fail closed.

The new seven-model boundary regression failed on the original guard and passes after the fix. Full local validation: **977 passed, 0 failed, 3 ignored**; formatting and diff checks pass. Deploy the combined verified changes as one Q2 candidate, then compare the full website matrix with the captured original Q2 and POMO rounds. The first image build is retained as an intermediate artifact and is not promoted to the domain.
