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
4. [done] Extend Q2 baseline to all seven models and inspect specific differences.
5. [done] Select evidence-backed compatibility fixes within frozen protocol/signature constraints; add regression tests, then implement.
6. [done] Local full validation and unchanged request-shape/general coding checks.
7. [done] Build immutable test-only image, deploy Q2 with rollback, repeat full matrix.
8. [done] Compare scores and POMO parity; compatibility and normal workload gates pass. Retain and disclose lower detector scores and unresolved attribution rather than claiming universal improvement.

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

The new seven-model boundary regression failed on the original guard and passes after the fix. Full local validation: **977 passed, 0 failed, 3 ignored**; formatting and diff checks pass. The combined verified changes were deployed as one Q2 candidate and compared with the original Q2 and POMO full matrices. The first image build remains an intermediate artifact and was not promoted to the domain.

## Deployment and final observations

Deployed source: `0391a916e6d6c850de66ba1524c9029f74a8f05d`.
Image: `ghcr.io/lusya123/kiro-rs:q2-test-0391a916e6d6c850de66ba1524c9029f74a8f05d`.
Digest: `sha256:e9b8212db6a63248a6c7519a3ebf57cc9fcb4b651628cfd312bcdb29e79cf8ba`.
CI: https://github.com/lusya123/kiro.rs/actions/runs/33972380334

The active container is `kiro-rs-q2-test-0391a91`, bound to `127.0.0.1:19006` on `43.156.115.199`. Its isolated config is `/home/ubuntu/services/q2-rollouts/20260905-0391a91-compat/config`. Nginx for `q2.quietfox.sbs` changed only its upstream port from 19005 to 19006 after config validation. The prior container `kiro-rs-q2-aws-b-68dd35c-proxy` remains available, with the Nginx rollback file at `/home/ubuntu/services/q2-rollouts/20260905-0391a91-compat/nginx.before.conf`. Final inspection found other container fingerprints unchanged.

Public-domain regression: 28 requests across all seven models passed arithmetic, generated code, exact context data, and stream stop checks. All request bodies matched the baseline byte for byte, all 28 response IDs retained `msg_bdrk_`, and seven generated Python functions passed three execution examples each. This is sampled evidence, not a guarantee for every future answer.

Each of POMO, Q2 before, and Q2 after has 28 registered model/site cells: 20 combinations with actual site controls were exercised, and 8 were explicitly recorded as unavailable. 9coding and API Dance used Bedrock standards; Ranking and CheckHub used their available Claude checks. No model substitution was used.

9coding changes: Opus 4.8 87.3→88.6, Opus 4.6 70.9→73.4, Sonnet 4.6 76.6→79.1; Opus 4.7 82.3→81.0, with a second run also 81.0. Other headline scores held. API Dance Sonnet 4.6 moved from HTTP 400/unscorable to 6/8. Ranking's displayed 100% differs from internal scores: Opus 5 76→84, Sonnet 5 84→83, Sonnet 4.6 84→83. These observations do not establish causation for every score difference.

For Opus 4.7, comparison of 42 identical request hashes identified an exact-copy punctuation difference (curly Chinese quotes became ASCII quotes). Both old and new local sanitizer versions preserved the original punctuation in ordinary/strict and five stream chunk configurations. Diagnostic tests were temporary and source was restored byte for byte; no punctuation-specific replacement was added. The upstream text and individual site grading details were unavailable, so attribution remains unresolved.

The artifact directory contains the complete report, three matrices, visible per-site details, actual downloaded JSON files, direct POMO/Q2 workload records, and 1,427 complete redacted HTTP exchanges (673 before, 754 after; no TCP gaps). Both temporary server captures were removed after local integrity validation. POMO's internal upstream server capture was not available; its original evidence is limited to site-visible material and direct reference requests. Final documentation commits do not change the deployed runtime source above.

## Iteration 3: input token accounting (2026-09-06)

The user's follow-up authorizes correcting token accounting while preserving the previous request, ID, signature, and ordinary-output constraints. Work continues on this isolated branch. Evidence is stored under `outputs/q2-token-accounting-20260906/` in the main checkout.

Two causes are reproduced: `TokenFeatures::add_text` appended an artificial newline to each supplied text field, and history accounting ignored tool-use names/arguments and tool-result text. The CheckHub text itself tokenizes to five local tokens; the invented newline made it six before the unchanged seven-token request/message framing. In 70 direct POMO/Q2 baseline requests, all returned HTTP 200. Q2's short and long tool-result requests reported identical input totals on every model; POMO's totals grew with the result content. POMO's standalone probe totals were inconsistent across models and runs, so they are retained as observations, not treated as a precise calibration table.

The fix counts independent supplied text segments without appending content and includes tool-use input and tool-result text in the shared request/count-tokens/cache-prefix estimator. It does not change the vocabulary, output token counter, actual request serialization, headers, parameters, response IDs, or signature handling. Image accounting remains separate and its regression suite still passes. No prompt-specific expected number or model-specific multiplier was added.

Three new regression tests failed on the old implementation and pass after the fix, covering all seven models, string/block representations, actual newlines, system fields, tool arguments/results and cache-prefix parity. The full local suite reports **980 passed, 0 failed, 3 ignored**. Older tests of retained diagnostic calibration curves had snapshots based on the old input accounting; their snapshots and labels were updated, while their production functions were left byte-identical. Public usage continues to bypass those legacy curves.

The local estimator remains approximate. The embedded ctoc vocabulary describes Claude 3–4.6; it cannot establish an exact Opus 4.7 tokenizer or justify forcing this probe to 17. References: https://github.com/rohangpta/ctoc and https://platform.claude.com/docs/en/build-with-claude/token-counting . Exact newer-model tokenization remains an explicit limitation.

### Iteration 3 deployment and measured results

Runtime source: `bd581c8a3387da3cc7f689e7cea083afd1a4be4c`; image `ghcr.io/lusya123/kiro-rs:q2-test-bd581c8a3387da3cc7f689e7cea083afd1a4be4c`; digest `sha256:3f7149419aada8eb76e40ae5075e622e9c0bba350c6f9f8c20f5ca1434ae60ea`. CI run: https://github.com/lusya123/kiro.rs/actions/runs/33979512171 . Q2 now targets `kiro-rs-q2-token-bd581c8` at `127.0.0.1:19007`, with isolated config under `/home/ubuntu/services/q2-rollouts/20260906-bd581c8-token/config`. The previous container and Nginx backup are retained. No production AWS-B branch or image alias was changed.

The canary passed 35 direct token requests and all 28 ordinary workload checks; all seven generated functions passed three execution examples each. Effective-payload counts matched the internal count endpoint. Public AWS-B Kiro count_tokens intentionally remains HTTP 404. The internal bare tool count excludes the existing 25-token tool preamble; the diagnostic counted that same effective input without changing inference requests.

The public-domain workload check had 28 HTTP 200 responses and 27/28 strict assertions. One Opus 5 context response preserved all values but returned ticket `"9173"` as a string. Three identical rechecks returned integer, integer, string, with all values intact. The original failed integer assertion and all responses are retained; no response rewriting was added. All seven public generated functions executed correctly.

All 20 site-supported combinations completed, with eight unsupported selector cells explicitly registered. CheckHub: Opus 5 81→85, Opus 4.8 79→91, Opus 4.7 85→85. The first two token probes now report 12 and pass; Opus 4.7 still fails the site's expected 17. 9coding Sonnet 4.6 changed 79.1→80.4; its other six headline scores held. Dance held at 5/8 for Opus 5 and Haiku, 6/8 for the other five models under matching configurations. Haiku's prior official JSON used explicit thinking; an additional adaptive run returned 1/8 without a signature and is preserved separately. Ranking page scores remain 100, while internal scores changed Opus 5 84→84, Sonnet 5 83→82, Sonnet 4.6 83→84.

The first CheckHub Opus 5 cache test had two creations, costing eight points. Six byte-identical requests show the first HTTP 200 contained only an empty thinking block and signature; the second had an answer, and the remaining four read the cache. Existing cache completion guards were preserved. Ingress HTTP does not establish why the first answer was absent; this issue is not claimed fixed.

An independent Opus 5 rerun scored 90, but only its first cache sample succeeded. At 2026-09-05 17:25:30 UTC, the dedicated zeno PRO+ credential reached upstream HTTP 402 `MONTHLY_REQUEST_COUNT` and was disabled as `QuotaExceeded`; the following five cache samples returned 502. **Q2 currently needs a funded dedicated test credential before inference can resume.** The main 20-combination matrix finished before quota exhaustion. The 90-point rerun is not evidence that cache reliability was fixed, and production credentials were not borrowed or altered.
