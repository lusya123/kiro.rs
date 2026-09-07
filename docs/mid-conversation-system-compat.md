# Mid-conversation system message compatibility

AWS-B rejected valid Claude Code requests such as `user → system` with HTTP 400 before contacting the upstream. The same validator serves `/v1/messages` and `/cc/v1/messages`.

The fix accepts correctly placed system messages on all supported Claude models, including Sonnet 5 and Sonnet 4.6, including consecutive reminders and reminders after user tool results. Leading system messages, invalid placement and assistant prefill remain rejected. Native Bedrock receives the original message JSON and extension fields unchanged.

Legacy Kiro supports only user/assistant turns. Text system messages are rendered as `<system-reminder>` blocks appended to the preceding user turn, before signature checks, token accounting, cache handling and wire conversion. Earlier turns, the top-level system prompt, text block cache controls and tool-result IDs are preserved. `clear_at: next_user_message` reminders stop rendering after a later user turn. This is a compatibility rendering and cannot guarantee native system-role priority. Tool additions/removals, per-message output configuration and non-text system blocks remain explicit errors on the legacy transport rather than being silently discarded.

Validation includes the formerly failing HTTP request on both endpoints, streaming and non-streaming completion through a simulated Kiro upstream, wire-level tool pairing and reminder order, scoped-reminder expiry, invalid inputs and native body preservation. These tests do not establish a successful production rollout or full support for the separate tool-change beta.

Reference: https://platform.claude.com/docs/en/build-with-claude/mid-conversation-system-messages


## Branch and routing boundary

The `sub2api的分组` branch is based on the external compatibility variant. It is released as `sub2api-compat-*`, independently of `aws-b` and `aws-b-external` aliases. The standard `aws-b` branch intentionally retains strict validation and must not receive this patch.

Sub2API customer traffic must not mix the standard fleet with this compatibility fleet. A release must audit actual account base URLs against live cluster membership, persistently exclude standard accounts from customer scheduling (including automatic recovery), and preserve unrelated accounts, credentials, groups, databases, Redis and production/staging application identities.


## Sub2API group cutover

Run `scripts/sub2api_group_audit.py` read-only on the Docker host, then run
`scripts/sub2api_group_policy.py AUDIT.json PLAN.json` locally. The audit resolves
actual account destinations against live cache roles and external source labels;
account names do not determine cluster identity. It includes standard source
ports whose containers are currently missing but whose external twins exist.
The plan contains bounded, explicit admin API requests and per-account rollback
states, with no credentials or request content.

For this customer-facing routing policy, standard accounts must be both
`status=inactive` and `schedulable=false`. Leaving status active allows an
ordinary successful health check to turn scheduling back on. Preserve standard
containers and their strict validator. Keep their account records, credentials
and group bindings for audit/rollback. Never let future credential import or
route recovery enable these standard destinations in Sub2API. Credential parity
between twins remains a separate invariant; it does not require both variants
to receive customer traffic. Other gateways are outside this cutover scope.

The planner refuses overlapping account IDs/ports or a customer group without
an eligible external account. This is a routing precondition, not proof of live
capacity: verify each affected model on the actual new image before applying.
Re-audit immediately before mutation; abort if the target account identities,
base URLs, status or group sets differ from the reviewed snapshot. Apply through
`POST /api/v1/admin/accounts/bulk-update`; check every per-account result, since
HTTP 200 alone does not imply the whole batch succeeded. On partial failure,
stop and restore only the successfully changed accounts to their saved state,
provided no subsequent administrator has changed them. Do not restart Sub2API.

A release is complete only after external-image rollout, persistent routing
isolation and successful gateway-level tests. Local tests and a pushed image
alone do not establish resolution of the production incident.
