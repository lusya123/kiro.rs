#!/usr/bin/env bash

set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  capture-ztest-report.sh REPORT_ID_OR_URL [OUT_DIR]
  capture-ztest-report.sh --input REPORT_RAW_JSON [OUT_DIR]

Downloads a live ZTest report, or processes an already captured API response.
The output keeps the byte-for-byte API response plus normalized failure files.
EOF
}

require_command() {
  local command_name="$1"

  if ! command -v "$command_name" >/dev/null 2>&1; then
    printf 'required command not found: %s\n' "$command_name" >&2
    exit 2
  fi
}

sha256_file() {
  local path="$1"

  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$path" | awk '{print $1}'
  else
    shasum -a 256 "$path" | awk '{print $1}'
  fi
}

if [[ $# -lt 1 || "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
  usage
  [[ $# -ge 1 ]] && exit 0
  exit 2
fi

require_command curl
require_command jq

SOURCE_KIND="api"
INPUT_PATH=""
REPORT_REF=""
OUT_DIR=""

if [[ "$1" == "--input" ]]; then
  if [[ $# -lt 2 || $# -gt 3 ]]; then
    usage >&2
    exit 2
  fi
  SOURCE_KIND="file"
  INPUT_PATH="$2"
  OUT_DIR="${3:-}"
  if [[ ! -f "$INPUT_PATH" ]]; then
    printf 'input report not found: %s\n' "$INPUT_PATH" >&2
    exit 2
  fi
  REPORT_ID="$(jq -r '.data.id // .id // empty' "$INPUT_PATH")"
  if [[ -z "$REPORT_ID" && "$INPUT_PATH" =~ ([0-9A-HJKMNP-TV-Z]{26}) ]]; then
    REPORT_ID="${BASH_REMATCH[1]}"
  fi
else
  if [[ $# -gt 2 ]]; then
    usage >&2
    exit 2
  fi
  REPORT_REF="$1"
  OUT_DIR="${2:-}"
  REPORT_ID="${REPORT_REF%%\?*}"
  REPORT_ID="${REPORT_ID%%\#*}"
  REPORT_ID="${REPORT_ID%/}"
  REPORT_ID="${REPORT_ID##*/}"
fi

if [[ -z "$REPORT_ID" || ! "$REPORT_ID" =~ ^[0-9A-HJKMNP-TV-Z]{26}$ ]]; then
  printf 'invalid or missing ZTest report ID: %s\n' "${REPORT_ID:-<empty>}" >&2
  exit 2
fi

REPORT_API_URL="https://ztest.ai/api/reports/${REPORT_ID}"
REPORT_PAGE_URL="https://ztest.ai/report/${REPORT_ID}"
CAPTURED_AT="$(date -u +'%Y-%m-%dT%H:%M:%SZ')"

if [[ -z "$OUT_DIR" ]]; then
  OUT_DIR="test-artifacts/ztest/reports/$(date +%F)-ztest-${REPORT_ID}"
fi
mkdir -p "$OUT_DIR"

RAW_PATH="$OUT_DIR/report.raw.json"
TEMP_PATH="$(mktemp)"
trap 'rm -f "$TEMP_PATH"' EXIT

if [[ "$SOURCE_KIND" == "api" ]]; then
  CURL_OK=1
  HTTP_STATUS="$(
    curl \
      --silent \
      --show-error \
      --location \
      --max-time 30 \
      --output "$TEMP_PATH" \
      --write-out '%{http_code}' \
      "$REPORT_API_URL"
  )" || CURL_OK=0
  cp "$TEMP_PATH" "$RAW_PATH"
  printf '%s\n' "$HTTP_STATUS" > "$OUT_DIR/http-status.txt"

  if [[ "$CURL_OK" != "1" || "$HTTP_STATUS" != "200" ]]; then
    printf 'ZTest report fetch failed (HTTP %s); raw response saved to %s\n' \
      "${HTTP_STATUS:-000}" "$RAW_PATH" >&2
    exit 1
  fi
else
  cp "$INPUT_PATH" "$RAW_PATH"
  printf '%s\n' "local-input" > "$OUT_DIR/http-status.txt"
fi

if ! jq empty "$RAW_PATH" >/dev/null 2>&1; then
  printf 'ZTest response is not valid JSON: %s\n' "$RAW_PATH" >&2
  exit 1
fi

if ! jq -e '
  type == "object"
  and ((.code? == null) or (.code == 0))
  and (((.data? | type) == "object") or ((.probe_results? | type) == "array"))
' "$RAW_PATH" >/dev/null; then
  printf 'ZTest response is an API error or lacks report data: %s\n' "$RAW_PATH" >&2
  exit 1
fi

jq 'if ((.data? | type) == "object") then .data else . end' \
  "$RAW_PATH" > "$OUT_DIR/report.data.json"

jq '
  def full_result:
    (.score == 100)
    and ((.status // "" | ascii_downcase) as $status
      | ($status == "pass" or $status == "passed" or $status == "success"));
  (.probe_results // []) | map(select(full_result | not))
' "$OUT_DIR/report.data.json" > "$OUT_DIR/non-full-probes.json"

jq '
  (.probe_results // [])
  | map(select(
      .error != null
      or .diagnosis != null
      or .details.error? != null
      or .details.network_error? != null
      or .details.skip_reason? != null
      or ((.status // "" | ascii_downcase) as $status
        | ($status == "failure" or $status == "failed"
          or $status == "partial" or $status == "timeout"))
    ))
' "$OUT_DIR/report.data.json" > "$OUT_DIR/exceptions.json"

jq -r '
  def flat:
    if . == null then ""
    elif type == "string" then gsub("[\\t\\r\\n]+"; " ")
    else tojson
    end;
  (["probe_code", "probe_name", "status", "score", "latency_ms", "label", "error", "diagnosis"] | @tsv),
  (.[] | [
    (.probe_code | flat),
    (.probe_name | flat),
    (.status | flat),
    (.score | flat),
    (.latency_ms | flat),
    (.label | flat),
    (.error | flat),
    (.diagnosis | flat)
  ] | @tsv)
' "$OUT_DIR/non-full-probes.json" > "$OUT_DIR/non-full-probes.tsv"

TOTAL_PROBES="$(jq '(.probe_results // []) | length' "$OUT_DIR/report.data.json")"
NON_FULL_PROBES="$(jq 'length' "$OUT_DIR/non-full-probes.json")"
EXCEPTION_PROBES="$(jq 'length' "$OUT_DIR/exceptions.json")"
RAW_SHA256="$(sha256_file "$RAW_PATH")"

jq -n \
  --arg captured_at "$CAPTURED_AT" \
  --arg source_kind "$SOURCE_KIND" \
  --arg report_id "$REPORT_ID" \
  --arg report_page_url "$REPORT_PAGE_URL" \
  --arg report_api_url "$REPORT_API_URL" \
  --arg raw_sha256 "$RAW_SHA256" \
  --argjson total_probes "$TOTAL_PROBES" \
  --argjson non_full_probes "$NON_FULL_PROBES" \
  --argjson exception_probes "$EXCEPTION_PROBES" '{
    captured_at: $captured_at,
    source_kind: $source_kind,
    report_id: $report_id,
    report_page_url: $report_page_url,
    report_api_url: $report_api_url,
    raw_sha256: $raw_sha256,
    total_probes: $total_probes,
    non_full_probes: $non_full_probes,
    exception_probes: $exception_probes,
    files: {
      raw: "report.raw.json",
      normalized: "report.data.json",
      non_full: "non-full-probes.json",
      non_full_tsv: "non-full-probes.tsv",
      exceptions: "exceptions.json"
    }
  }' > "$OUT_DIR/manifest.json"

jq -r \
  --arg captured_at "$CAPTURED_AT" \
  --arg report_page_url "$REPORT_PAGE_URL" \
  --arg raw_sha256 "$RAW_SHA256" \
  --argjson non_full_count "$NON_FULL_PROBES" '
  def cell:
    if . == null then ""
    elif type == "string" then gsub("[|\\t\\r\\n]+"; " ")
    else tojson
    end;
  . as $report
  | "# ZTest report " + ($report.id // "unknown")
    + "\n\n- Report: <" + $report_page_url + ">"
    + "\n- Captured: `" + $captured_at + "`"
    + "\n- Status: `" + (($report.status // "unknown") | tostring) + "`"
    + "\n- Risk level: `" + (($report.risk_level // "unknown") | tostring) + "`"
    + "\n- Composite score: `" + (($report.composite_score // "n/a") | tostring) + "`"
    + "\n- Model: `" + (($report.model.code // $report.model // "unknown") | tostring) + "`"
    + "\n- Raw SHA-256: `" + $raw_sha256 + "`"
    + "\n- Non-full probes: `" + ($non_full_count | tostring) + "`"
    + "\n\nThe exact API response is preserved in `report.raw.json`."
    + " Diagnostic subsets are in `non-full-probes.json` and `exceptions.json`."
    + "\n\n## Non-Full Probes\n\n"
    + "| Probe | Name | Status | Score | Latency ms | Error | Diagnosis |\n"
    + "| --- | --- | --- | ---: | ---: | --- | --- |\n"
    + ((($report.probe_results // [])
      | map(select(
          ((.score == 100)
            and ((.status // "" | ascii_downcase) as $status
              | ($status == "pass" or $status == "passed" or $status == "success")))
          | not
        ))
      | map("| "
        + (.probe_code | cell) + " | "
        + (.probe_name | cell) + " | "
        + (.status | cell) + " | "
        + (.score | cell) + " | "
        + (.latency_ms | cell) + " | "
        + (.error | cell) + " | "
        + (.diagnosis | cell) + " |")
      | join("\n")) // "")
' "$OUT_DIR/report.data.json" > "$OUT_DIR/README.md"

printf 'captured ZTest report %s\n' "$REPORT_ID"
printf 'output: %s\n' "$OUT_DIR"
printf 'probes: %s total, %s non-full, %s with exceptions\n' \
  "$TOTAL_PROBES" "$NON_FULL_PROBES" "$EXCEPTION_PROBES"
