# POMO live-suite reference

Date: 2026-07-15 (Asia/Shanghai)

Target: `https://www.pomoai.ai`

Model: `claude-opus-4-8`

This directory is the same live suite used against Q2. API keys are not
stored. All request JSON, response headers, response bodies, SSE, status codes,
timings, and concurrent outputs are preserved verbatim.

The reference returned HTTP 200 for normal text, exact JSON, thinking stream,
forced tools, streamed tools, identity JSON, both image prompts, tool results,
and all ten concurrent calls. It returned the same 401/403/403/400 error
boundaries and the same intentional public count-tokens 404 as Q2.

Prompt-cache routing was nondeterministic in this run: both identical calls
reported 3404 cache-creation tokens and zero cache-read tokens. Q2's shared
cache produced a deterministic creation followed by a read for the same pair.

These samples prove the reference format for successful complex calls. They do
not make Q2's corresponding 502s a code-format failure; Q2's raw bodies contain
an explicit AWS account restriction before a successful model response exists
to convert.
