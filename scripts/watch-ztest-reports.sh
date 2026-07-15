#!/usr/bin/env bash

set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  watch-ztest-reports.sh [options]

Watches Codex desktop log increments for newly opened ZTest report URLs and
immediately archives each report with capture-ztest-report.sh.

Options:
  --log-root DIR       Codex desktop log root
  --output-root DIR    ZTest report artifact root
  --state-file FILE    File containing already processed report IDs
  --scan-existing      Also process existing log content (default: new lines)
  --stdin              Read candidate log lines from stdin, then exit
  -h, --help           Show this help

Environment:
  ZTEST_CAPTURE_SCRIPT Override the capture script path (useful for tests)
EOF
}

require_command() {
  local command_name="$1"

  if ! command -v "$command_name" >/dev/null 2>&1; then
    printf 'required command not found: %s\n' "$command_name" >&2
    exit 2
  fi
}

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
LOG_ROOT="$HOME/Library/Logs/com.openai.codex"
OUTPUT_ROOT="$REPO_ROOT/test-artifacts/ztest/reports"
STATE_FILE=""
SCAN_EXISTING=0
INPUT_MODE="logs"
CAPTURE_SCRIPT="${ZTEST_CAPTURE_SCRIPT:-$SCRIPT_DIR/capture-ztest-report.sh}"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --log-root)
      [[ $# -ge 2 ]] || { usage >&2; exit 2; }
      LOG_ROOT="$2"
      shift 2
      ;;
    --output-root)
      [[ $# -ge 2 ]] || { usage >&2; exit 2; }
      OUTPUT_ROOT="$2"
      shift 2
      ;;
    --state-file)
      [[ $# -ge 2 ]] || { usage >&2; exit 2; }
      STATE_FILE="$2"
      shift 2
      ;;
    --scan-existing)
      SCAN_EXISTING=1
      shift
      ;;
    --stdin)
      INPUT_MODE="stdin"
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      printf 'unknown option: %s\n' "$1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

if [[ ! -x "$CAPTURE_SCRIPT" ]]; then
  printf 'capture script is not executable: %s\n' "$CAPTURE_SCRIPT" >&2
  exit 2
fi

require_command find
require_command mkfifo
require_command mktemp
require_command tail

mkdir -p "$OUTPUT_ROOT"
STATE_FILE="${STATE_FILE:-$OUTPUT_ROOT/.ztest-report-watch.seen}"
mkdir -p "$(dirname "$STATE_FILE")"
touch "$STATE_FILE"

LOCK_DIR="${STATE_FILE}.lock"
if ! mkdir "$LOCK_DIR" 2>/dev/null; then
  printf 'another ZTest report watcher is using state file: %s\n' \
    "$STATE_FILE" >&2
  exit 1
fi

TAIL_PID=""
PIPE_DIR=""

cleanup() {
  if [[ -n "$TAIL_PID" ]]; then
    kill "$TAIL_PID" 2>/dev/null || true
    wait "$TAIL_PID" 2>/dev/null || true
  fi
  if [[ -n "$PIPE_DIR" ]]; then
    rm -f "$PIPE_DIR/log-stream"
    rmdir "$PIPE_DIR" 2>/dev/null || true
  fi
  rmdir "$LOCK_DIR" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

report_seen() {
  local report_id="$1"
  local seen_id

  while IFS= read -r seen_id; do
    [[ "$seen_id" == "$report_id" ]] && return 0
  done < "$STATE_FILE"
  return 1
}

record_report() {
  local report_id="$1"

  printf '%s\n' "$report_id" >> "$STATE_FILE"
}

capture_report() {
  local report_id="$1"
  local report_url="https://ztest.ai/report/${report_id}"
  local output_dir="$OUTPUT_ROOT/$(date +%F)-ztest-${report_id}"
  local retry_delay

  if report_seen "$report_id"; then
    return 0
  fi

  printf '[%s] discovered ZTest report %s\n' \
    "$(date -u +'%Y-%m-%dT%H:%M:%SZ')" "$report_id"

  for retry_delay in 0 1 2 4 8 16; do
    if [[ "$retry_delay" -gt 0 ]]; then
      sleep "$retry_delay"
    fi

    if "$CAPTURE_SCRIPT" "$report_url" "$output_dir"; then
      record_report "$report_id"
      printf '[%s] archived ZTest report %s at %s\n' \
        "$(date -u +'%Y-%m-%dT%H:%M:%SZ')" "$report_id" "$output_dir"
      return 0
    fi

    printf '[%s] report %s capture attempt failed; retrying\n' \
      "$(date -u +'%Y-%m-%dT%H:%M:%SZ')" "$report_id" >&2
  done

  record_report "$report_id"
  printf '%s\t%s\t%s\n' \
    "$(date -u +'%Y-%m-%dT%H:%M:%SZ')" "$report_id" "capture_failed" \
    >> "${STATE_FILE}.failures.tsv"
  printf 'failed to archive ZTest report after retries: %s\n' \
    "$report_id" >&2
  return 0
}

process_line() {
  local line="$1"
  local remaining="$line"
  local report_id
  local matched_url

  while [[ "$remaining" =~ https://ztest\.ai/report/([0-9A-HJKMNP-TV-Z]{26}) ]]; do
    report_id="${BASH_REMATCH[1]}"
    matched_url="https://ztest.ai/report/${report_id}"
    capture_report "$report_id"
    remaining="${remaining#*"$matched_url"}"
  done
}

if [[ "$INPUT_MODE" == "stdin" ]]; then
  while IFS= read -r line; do
    process_line "$line"
  done
  exit 0
fi

if [[ ! -d "$LOG_ROOT" ]]; then
  printf 'Codex desktop log root not found: %s\n' "$LOG_ROOT" >&2
  exit 2
fi

LOG_FILES=()
while IFS= read -r log_file; do
  LOG_FILES+=("$log_file")
done < <(find "$LOG_ROOT" -type f -name '*.log' -mtime -2 -print)

if [[ "${#LOG_FILES[@]}" -eq 0 ]]; then
  printf 'no recent Codex desktop log files found under: %s\n' "$LOG_ROOT" >&2
  exit 2
fi

printf '[%s] watching %s Codex log file(s) for new ZTest reports\n' \
  "$(date -u +'%Y-%m-%dT%H:%M:%SZ')" "${#LOG_FILES[@]}"

if [[ "$SCAN_EXISTING" == "1" ]]; then
  TAIL_LINES="+1"
else
  TAIL_LINES="0"
fi

PIPE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/ztest-report-watch.XXXXXX")"
PIPE_PATH="$PIPE_DIR/log-stream"
mkfifo "$PIPE_PATH"

tail -F -n "$TAIL_LINES" "${LOG_FILES[@]}" > "$PIPE_PATH" &
TAIL_PID="$!"

while IFS= read -r line; do
  process_line "$line"
done < "$PIPE_PATH"
