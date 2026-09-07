# Mid-conversation system message compatibility

AWS-B rejected valid Claude Code requests such as `user → system` with HTTP 400 before contacting the upstream. The same validator serves `/v1/messages` and `/cc/v1/messages`.

The fix accepts correctly placed system messages on Opus 4.8 and Opus 5, including consecutive reminders and reminders after user tool results. Leading system messages, invalid placement and assistant prefill remain rejected. Native Bedrock receives the original message JSON and extension fields unchanged.

Legacy Kiro supports only user/assistant turns. Text system messages are rendered as `<system-reminder>` blocks appended to the preceding user turn, before signature checks, token accounting, cache handling and wire conversion. Earlier turns, the top-level system prompt, text block cache controls and tool-result IDs are preserved. `clear_at: next_user_message` reminders stop rendering after a later user turn. This is a compatibility rendering and cannot guarantee native system-role priority. Tool additions/removals, per-message output configuration and non-text system blocks remain explicit errors on the legacy transport rather than being silently discarded.

Validation includes the formerly failing HTTP request on both endpoints, streaming and non-streaming completion through a simulated Kiro upstream, wire-level tool pairing and reminder order, scoped-reminder expiry, invalid inputs and native body preservation. These tests do not establish a successful production rollout or full support for the separate tool-change beta.

Reference: https://platform.claude.com/docs/en/build-with-claude/mid-conversation-system-messages
