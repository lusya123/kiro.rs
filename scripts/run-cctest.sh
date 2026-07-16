#!/usr/bin/env bash

set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  CCTEST_API_KEY=... TARGET_API_KEY=... \
    run-cctest.sh ENDPOINT MODEL [OUT_DIR]
  CCTEST_API_KEY=... \
    run-cctest.sh --resume TASK_ID [OUT_DIR]

Submits or resumes a CCTest check, polls it to a terminal state, and preserves
the raw status history and final report. API keys are read from the environment
and are never copied into the output directory.

Optional environment variables:
  CCTEST_BASE_URL       API origin (default: https://cctest.ai)
  CCTEST_POLL_INTERVAL  Seconds between polls (default: 5)
  CCTEST_MAX_WAIT       Maximum poll time in seconds (default: 1800)
  CCTEST_CURL_MAX_TIME  Per-request timeout in seconds (default: 30)
EOF
}

require_command() {
  local command_name="$1"

  if ! command -v "$command_name" >/dev/null 2>&1; then
    printf 'required command not found: %s\n' "$command_name" >&2
    exit 2
  fi
}

is_non_negative_integer() {
  [[ "$1" =~ ^[0-9]+$ ]]
}

require_command curl
require_command jq
require_command rg

if [[ $# -lt 1 || "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  [[ $# -ge 1 ]] && exit 0
  exit 2
fi

if [[ -z "${CCTEST_API_KEY:-}" ]]; then
  printf 'CCTEST_API_KEY is required\n' >&2
  exit 2
fi
CCTEST_API_KEY_VALUE="$CCTEST_API_KEY"
unset CCTEST_API_KEY

BASE_URL="${CCTEST_BASE_URL:-https://cctest.ai}"
BASE_URL="${BASE_URL%/}"
POLL_INTERVAL="${CCTEST_POLL_INTERVAL:-5}"
MAX_WAIT="${CCTEST_MAX_WAIT:-1800}"
CURL_MAX_TIME="${CCTEST_CURL_MAX_TIME:-30}"

if ! is_non_negative_integer "$POLL_INTERVAL"; then
  printf 'CCTEST_POLL_INTERVAL must be a non-negative integer\n' >&2
  exit 2
fi
for positive_setting in "$MAX_WAIT" "$CURL_MAX_TIME"; do
  if ! is_non_negative_integer "$positive_setting" || (( positive_setting == 0 )); then
    printf 'CCTEST_MAX_WAIT and CCTEST_CURL_MAX_TIME must be positive integers\n' >&2
    exit 2
  fi
done

MODE="submit"
TASK_ID=""
ENDPOINT=""
MODEL=""
OUT_DIR=""
TARGET_API_KEY_VALUE=""

if [[ "$1" == "--resume" ]]; then
  MODE="resume"
  if [[ $# -lt 2 || $# -gt 3 ]]; then
    usage >&2
    exit 2
  fi
  TASK_ID="$2"
  OUT_DIR="${3:-}"
else
  if [[ $# -lt 2 || $# -gt 3 ]]; then
    usage >&2
    exit 2
  fi
  if [[ -z "${TARGET_API_KEY:-}" ]]; then
    printf 'TARGET_API_KEY is required when submitting a check\n' >&2
    exit 2
  fi
  TARGET_API_KEY_VALUE="$TARGET_API_KEY"
  unset TARGET_API_KEY
  ENDPOINT="$1"
  MODEL="$2"
  OUT_DIR="${3:-}"
fi

if [[ "$MODE" == "resume" && ! "$TASK_ID" =~ ^[0-9a-fA-F-]{36}$ ]]; then
  printf 'invalid CCTest task ID: %s\n' "$TASK_ID" >&2
  exit 2
fi
if [[ "$MODE" == "submit" && -n "$OUT_DIR" && -e "$OUT_DIR/manifest.json" ]]; then
  printf 'refusing to submit with an existing CCTest manifest: %s\n' \
    "$OUT_DIR/manifest.json" >&2
  exit 2
fi

umask 077
AUTH_HEADER_FILE="$(mktemp)"
REQUEST_FILE="$(mktemp)"
RESPONSE_FILE="$(mktemp)"
MANIFEST_TMP="$(mktemp)"
trap 'rm -f "$AUTH_HEADER_FILE" "$REQUEST_FILE" "$RESPONSE_FILE" "$MANIFEST_TMP"' EXIT

printf 'Authorization: Bearer %s\n' "$CCTEST_API_KEY_VALUE" > "$AUTH_HEADER_FILE"

curl_json() {
  local method="$1"
  local url="$2"
  local output="$3"
  local http_status

  if [[ "$method" == "POST" ]]; then
    http_status="$(
      curl \
        --silent \
        --show-error \
        --location \
        --connect-timeout 10 \
        --max-time "$CURL_MAX_TIME" \
        --request POST \
        --header "@$AUTH_HEADER_FILE" \
        --header 'Content-Type: application/json' \
        --data-binary "@$REQUEST_FILE" \
        --output "$output" \
        --write-out '%{http_code}' \
        "$url"
    )" || return 1
  else
    http_status="$(
      curl \
        --silent \
        --show-error \
        --location \
        --connect-timeout 10 \
        --max-time "$CURL_MAX_TIME" \
        --header "@$AUTH_HEADER_FILE" \
        --output "$output" \
        --write-out '%{http_code}' \
        "$url"
    )" || return 1
  fi

  [[ "$http_status" == "200" || "$http_status" == "201" || "$http_status" == "202" ]]
}

assert_response_secret_safe() {
  local response_path="$1"

  if rg -F -- "$CCTEST_API_KEY_VALUE" "$response_path" >/dev/null 2>&1; then
    printf 'secret-safety check failed: CCTest API key echoed in response\n' >&2
    exit 1
  fi
  if [[ -n "$TARGET_API_KEY_VALUE" ]] \
    && rg -F -- "$TARGET_API_KEY_VALUE" "$response_path" >/dev/null 2>&1; then
    printf 'secret-safety check failed: target API key echoed in response\n' >&2
    exit 1
  fi
}

if [[ "$MODE" == "submit" ]]; then
  printf '%s' "$TARGET_API_KEY_VALUE" | jq -Rsc \
    --arg url "$ENDPOINT" \
    --arg model "$MODEL" \
    '{url: $url, apiKey: ., model: $model}' > "$REQUEST_FILE"

  if ! curl_json POST "$BASE_URL/api/v1/check" "$RESPONSE_FILE"; then
    printf 'CCTest submission failed or timed out\n' >&2
    exit 1
  fi
  assert_response_secret_safe "$RESPONSE_FILE"

  TASK_ID="$(jq -r '.taskId // empty' "$RESPONSE_FILE")"
  if [[ ! "$TASK_ID" =~ ^[0-9a-fA-F-]{36}$ ]]; then
    printf 'CCTest submission did not return a valid task ID\n' >&2
    exit 1
  fi

  # The target key is needed only for submission. Remove its temporary payload
  # before creating any persistent evidence files.
  : > "$REQUEST_FILE"
fi

if [[ -z "$OUT_DIR" ]]; then
  OUT_DIR="test-artifacts/cctest/runs/$(date +%F)-${TASK_ID}"
fi
mkdir -p "$OUT_DIR"

MANIFEST_PATH="$OUT_DIR/manifest.json"
recorded_at="$(date -u +'%Y-%m-%dT%H:%M:%SZ')"

if [[ "$MODE" == "resume" && -f "$MANIFEST_PATH" ]]; then
  existing_task_id="$(jq -r '.taskId // empty' "$MANIFEST_PATH")"
  if [[ "$existing_task_id" != "$TASK_ID" ]]; then
    printf 'output directory belongs to a different CCTest task: %s\n' \
      "$existing_task_id" >&2
    exit 2
  fi
  jq --arg resumed_at "$recorded_at" \
    '. + {lastResumedAt: $resumed_at}' \
    "$MANIFEST_PATH" > "$MANIFEST_TMP"
  mv "$MANIFEST_TMP" "$MANIFEST_PATH"
elif [[ "$MODE" == "submit" && -e "$MANIFEST_PATH" ]]; then
  printf 'refusing to overwrite an existing CCTest manifest: %s\n' \
    "$MANIFEST_PATH" >&2
  exit 2
else
  jq -n \
    --arg task_id "$TASK_ID" \
    --arg mode "$MODE" \
    --arg endpoint "$ENDPOINT" \
    --arg model "$MODEL" \
    --arg recorded_at "$recorded_at" \
    '{
      taskId: $task_id,
      mode: $mode,
      endpoint: (if $endpoint == "" then null else $endpoint end),
      model: (if $model == "" then null else $model end),
      submittedAt: (if $mode == "submit" then $recorded_at else null end),
      lastResumedAt: (if $mode == "resume" then $recorded_at else null end)
    }' > "$MANIFEST_PATH"
fi

STATUS_HISTORY="$OUT_DIR/status.ndjson"
if [[ "$MODE" == "submit" ]]; then
  : > "$STATUS_HISTORY"
else
  touch "$STATUS_HISTORY"
fi

start_epoch="$(date +%s)"
attempt=0
terminal_status=""

while true; do
  attempt=$((attempt + 1))
  now_epoch="$(date +%s)"
  elapsed=$((now_epoch - start_epoch))

  if (( elapsed > MAX_WAIT )); then
    printf 'CCTest polling timed out after %ss; task can be resumed: %s\n' \
      "$elapsed" "$TASK_ID" >&2
    printf '%s\n' "$TASK_ID" > "$OUT_DIR/task-id.txt"
    exit 124
  fi

  : > "$RESPONSE_FILE"
  if curl_json GET "$BASE_URL/api/v1/check/$TASK_ID" "$RESPONSE_FILE" \
    && assert_response_secret_safe "$RESPONSE_FILE" \
    && jq empty "$RESPONSE_FILE" >/dev/null 2>&1; then
    captured_at="$(date -u +'%Y-%m-%dT%H:%M:%SZ')"
    jq -c \
      --arg captured_at "$captured_at" \
      --argjson attempt "$attempt" \
      '. + {capturedAt: $captured_at, pollAttempt: $attempt}' \
      "$RESPONSE_FILE" >> "$STATUS_HISTORY"

    status="$(jq -r '.status // "unknown"' "$RESPONSE_FILE")"
    step_name="$(jq -r '.stepName // empty' "$RESPONSE_FILE")"
    progress="$(jq -r '.progress // empty' "$RESPONSE_FILE")"
    printf 'task=%s status=%s step=%s progress=%s elapsed=%ss\n' \
      "$TASK_ID" "$status" "${step_name:--}" "${progress:--}" "$elapsed"

    case "$status" in
      done|completed|failed|error|cancelled|canceled)
        terminal_status="$status"
        cp "$RESPONSE_FILE" "$OUT_DIR/report.raw.json"
        break
        ;;
    esac
  else
    printf 'task=%s poll_attempt=%s unavailable elapsed=%ss\n' \
      "$TASK_ID" "$attempt" "$elapsed" >&2
  fi

  sleep "$POLL_INTERVAL"
done

printf '%s\n' "$TASK_ID" > "$OUT_DIR/task-id.txt"

jq '{
  status,
  verdictKey,
  totalScore,
  expectedModel,
  channel: (.channel // .channelType // null),
  scores,
  metrics,
  stepStatus,
  details,
  checks,
  taskId: (.taskId // null)
}' "$OUT_DIR/report.raw.json" > "$OUT_DIR/summary.json"

if rg -l -F -- "$CCTEST_API_KEY_VALUE" "$OUT_DIR" >/dev/null 2>&1; then
  printf 'secret-safety check failed: CCTest API key found in output directory\n' >&2
  exit 1
fi
if [[ -n "$TARGET_API_KEY_VALUE" ]] \
  && rg -l -F -- "$TARGET_API_KEY_VALUE" "$OUT_DIR" >/dev/null 2>&1; then
  printf 'secret-safety check failed: target API key found in output directory\n' >&2
  exit 1
fi

printf 'CCTest task %s finished with status=%s; report saved to %s\n' \
  "$TASK_ID" "$terminal_status" "$OUT_DIR/report.raw.json"

if [[ "$terminal_status" != "done" && "$terminal_status" != "completed" ]]; then
  exit 1
fi
