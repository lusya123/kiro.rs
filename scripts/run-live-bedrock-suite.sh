#!/usr/bin/env bash

set -euo pipefail

: "${BASE_URL:?set BASE_URL, for example https://q2.example.com}"
: "${API_KEY:?set API_KEY}"

MODEL="${MODEL:-claude-opus-4-8}"
OUT_DIR="${OUT_DIR:-test-artifacts/ztest/live-$(date +%Y%m%d-%H%M%S)}"
FIXTURE_DIR="$(cd "$(dirname "$0")/.." && pwd)/test-artifacts/ztest/live-suite-fixtures"
IMAGE_FILE="${IMAGE_FILE:-$(cd "$(dirname "$0")/.." && pwd)/test-artifacts/ztest/direct-parity/2026-07-15-post-fix/aws-b-e2e-image-100x100.png}"
SPATIAL_FILE="${SPATIAL_FILE:-$(cd "$(dirname "$0")/.." && pwd)/test-artifacts/ztest/direct-parity/2026-07-15-post-fix/aws-b-e2e-spatial.png}"

mkdir -p "$OUT_DIR/requests" "$OUT_DIR/responses"
RESULTS="$OUT_DIR/results.tsv"
ASSERTIONS="$OUT_DIR/assertions.tsv"
printf 'name\thttp_code\tcurl_exit\ttime_total\tsize_download\n' > "$RESULTS"
printf 'assertion\tresult\tdetail\n' > "$ASSERTIONS"

auth_headers=(
  -H "Authorization: Bearer $API_KEY"
  -H "x-api-key: $API_KEY"
  -H 'anthropic-version: 2023-06-01'
  -H 'content-type: application/json'
)

record_post() {
  local name="$1"
  local path="$2"
  local request_file="$3"
  shift 3
  local meta rc code total size

  set +e
  meta=$(curl --http2 -sS --max-time 180 \
    -D "$OUT_DIR/responses/$name.headers" \
    -o "$OUT_DIR/responses/$name.json" \
    -w '%{http_code}\t%{time_total}\t%{size_download}' \
    "$@" \
    --data-binary "@$request_file" \
    "$BASE_URL$path")
  rc=$?
  set -e
  IFS=$'\t' read -r code total size <<< "$meta"
  printf '%s\t%s\t%s\t%s\t%s\n' "$name" "${code:-000}" "$rc" "${total:-0}" "${size:-0}" >> "$RESULTS"
}

record_stream() {
  local name="$1"
  local path="$2"
  local request_file="$3"
  local meta rc code total size

  set +e
  meta=$(curl --http2 -sS -N --max-time 240 \
    -D "$OUT_DIR/responses/$name.headers" \
    -o "$OUT_DIR/responses/$name.sse" \
    -w '%{http_code}\t%{time_total}\t%{size_download}' \
    "${auth_headers[@]}" \
    --data-binary "@$request_file" \
    "$BASE_URL$path")
  rc=$?
  set -e
  IFS=$'\t' read -r code total size <<< "$meta"
  printf '%s\t%s\t%s\t%s\t%s\n' "$name" "${code:-000}" "$rc" "${total:-0}" "${size:-0}" >> "$RESULTS"
}

assert_eq() {
  local name="$1"
  local actual="$2"
  local expected="$3"
  if [[ "$actual" == "$expected" ]]; then
    printf '%s\tPASS\t%s\n' "$name" "$actual" >> "$ASSERTIONS"
  else
    printf '%s\tFAIL\texpected=%s actual=%s\n' "$name" "$expected" "$actual" >> "$ASSERTIONS"
  fi
}

assert_true() {
  local name="$1"
  local detail="$2"
  shift 2
  if "$@"; then
    printf '%s\tPASS\t%s\n' "$name" "$detail" >> "$ASSERTIONS"
  else
    printf '%s\tFAIL\t%s\n' "$name" "$detail" >> "$ASSERTIONS"
  fi
}

cp "$FIXTURE_DIR"/*.json "$OUT_DIR/requests/"

jq --arg model "$MODEL" '.model = $model' "$FIXTURE_DIR/exact-pong.json" > "$OUT_DIR/requests/exact-pong.json"
jq '.stream = true' "$OUT_DIR/requests/exact-pong.json" > "$OUT_DIR/requests/exact-pong-stream.json"
jq --arg model "$MODEL" '.model = $model' "$FIXTURE_DIR/exact-json.json" > "$OUT_DIR/requests/exact-json.json"
jq --arg model "$MODEL" '.model = $model' "$FIXTURE_DIR/thinking-stream.json" > "$OUT_DIR/requests/thinking-stream.json"
jq --arg model "$MODEL" '.model = $model' "$FIXTURE_DIR/tool.json" > "$OUT_DIR/requests/tool.json"
jq '.stream = true' "$OUT_DIR/requests/tool.json" > "$OUT_DIR/requests/tool-stream.json"
jq --arg model "$MODEL" '.model = $model' "$FIXTURE_DIR/identity.json" > "$OUT_DIR/requests/identity.json"
jq --arg model "$MODEL" '.model = $model' "$FIXTURE_DIR/adversarial-system.json" > "$OUT_DIR/requests/adversarial-system.json"

cache_anchor=$(for i in $(seq 1 170); do printf 'stable cache anchor segment %s: protocol parity datum. ' "$i"; done)
jq -n --arg model "$MODEL" --arg anchor "$cache_anchor" '{
  model: $model,
  max_tokens: 32,
  system: [{type: "text", text: $anchor, cache_control: {type: "ephemeral"}}],
  messages: [{role: "user", content: "Reply exactly CACHE_OK."}]
}' > "$OUT_DIR/requests/cache.json"

image_data=$(base64 < "$IMAGE_FILE" | tr -d '\n')
jq -n --arg model "$MODEL" --arg data "$image_data" '{
  model: $model,
  max_tokens: 32,
  messages: [{role: "user", content: [
    {type: "image", source: {type: "base64", media_type: "image/png", data: $data}},
    {type: "text", text: "What color is this image? Reply with one color word only."}
  ]}]
}' > "$OUT_DIR/requests/image.json"
unset image_data

spatial_data=$(base64 < "$SPATIAL_FILE" | tr -d '\n')
jq -n --arg model "$MODEL" --arg data "$spatial_data" '{
  model: $model,
  max_tokens: 32,
  messages: [{role: "user", content: [
    {type: "image", source: {type: "base64", media_type: "image/png", data: $data}},
    {type: "text", text: "What color is the circle on the left? Reply with one color word only."}
  ]}]
}' > "$OUT_DIR/requests/spatial.json"
unset spatial_data cache_anchor

curl --http2 -sS --max-time 30 \
  -D "$OUT_DIR/responses/models.headers" \
  -o "$OUT_DIR/responses/models.json" \
  -w "models\t%{http_code}\t0\t%{time_total}\t%{size_download}\n" \
  "${auth_headers[@]}" \
  "$BASE_URL/v1/models" >> "$RESULTS"

record_post noauth /v1/messages "$OUT_DIR/requests/exact-pong.json" -H 'content-type: application/json'
record_post badauth /v1/messages "$OUT_DIR/requests/exact-pong.json" -H 'Authorization: Bearer sk-invalid-live-suite' -H 'content-type: application/json'
record_post badmodel /v1/messages "$FIXTURE_DIR/bad-model.json" "${auth_headers[@]}"
record_post malformed /v1/messages "$FIXTURE_DIR/malformed.json" "${auth_headers[@]}"
record_post exact-pong /v1/messages "$OUT_DIR/requests/exact-pong.json" "${auth_headers[@]}"
record_stream exact-pong-stream /v1/messages "$OUT_DIR/requests/exact-pong-stream.json"
record_post exact-json /v1/messages "$OUT_DIR/requests/exact-json.json" "${auth_headers[@]}"
record_stream thinking-stream /v1/messages "$OUT_DIR/requests/thinking-stream.json"
record_post tool /v1/messages "$OUT_DIR/requests/tool.json" "${auth_headers[@]}"
record_stream tool-stream /v1/messages "$OUT_DIR/requests/tool-stream.json"
record_post identity /v1/messages "$OUT_DIR/requests/identity.json" "${auth_headers[@]}"
record_post adversarial-system /v1/messages "$OUT_DIR/requests/adversarial-system.json" "${auth_headers[@]}"
record_post cache-create /v1/messages "$OUT_DIR/requests/cache.json" "${auth_headers[@]}"
record_post cache-read /v1/messages "$OUT_DIR/requests/cache.json" "${auth_headers[@]}"
record_post image /v1/messages "$OUT_DIR/requests/image.json" "${auth_headers[@]}"
record_post spatial /v1/messages "$OUT_DIR/requests/spatial.json" "${auth_headers[@]}"
record_post count-tokens /v1/messages/count_tokens "$OUT_DIR/requests/exact-json.json" "${auth_headers[@]}"

tool_id=$(jq -r '.content[]? | select(.type == "tool_use") | .id' "$OUT_DIR/responses/tool.json" | head -1)
if [[ -n "$tool_id" ]]; then
  jq -n --arg model "$MODEL" --arg id "$tool_id" '{
    model: $model,
    max_tokens: 64,
    messages: [
      {role: "user", content: "Call get_weather for Paris with unit celsius. Return only the tool call."},
      {role: "assistant", content: [{type: "tool_use", id: $id, name: "get_weather", input: {location: "Paris", unit: "celsius"}}]},
      {role: "user", content: [{type: "tool_result", tool_use_id: $id, content: "18 C and clear"}]}
    ]
  }' > "$OUT_DIR/requests/tool-result.json"
  record_post tool-result /v1/messages "$OUT_DIR/requests/tool-result.json" "${auth_headers[@]}"
fi

mkdir -p "$OUT_DIR/responses/concurrent"
for i in $(seq 1 10); do
  (
    set +e
    meta=$(curl --http2 -sS --max-time 180 \
      -o "$OUT_DIR/responses/concurrent/$i.json" \
      -w '%{http_code}\t%{time_total}\t%{size_download}' \
      "${auth_headers[@]}" \
      --data-binary "@$OUT_DIR/requests/exact-pong.json" \
      "$BASE_URL/v1/messages")
    rc=$?
    printf '%s\t%s\n' "$rc" "$meta" > "$OUT_DIR/responses/concurrent/$i.meta"
  ) &
done
wait

for i in $(seq 1 10); do
  IFS=$'\t' read -r rc code total size < "$OUT_DIR/responses/concurrent/$i.meta"
  printf 'concurrent-%s\t%s\t%s\t%s\t%s\n' "$i" "$code" "$rc" "$total" "$size" >> "$RESULTS"
done

assert_eq models_http "$(awk -F '\t' '$1 == "models" {print $2}' "$RESULTS")" 200
assert_eq noauth_http "$(awk -F '\t' '$1 == "noauth" {print $2}' "$RESULTS")" 401
assert_eq badauth_http "$(awk -F '\t' '$1 == "badauth" {print $2}' "$RESULTS")" 403
assert_eq public_count_tokens_http "$(awk -F '\t' '$1 == "count-tokens" {print $2}' "$RESULTS")" 404
assert_eq exact_pong_text "$(jq -r '.content[0].text // ""' "$OUT_DIR/responses/exact-pong.json")" pong
assert_eq exact_pong_output_tokens "$(jq -r '.usage.output_tokens // -1' "$OUT_DIR/responses/exact-pong.json")" 4
assert_true bedrock_message_id "$(jq -r '.id // ""' "$OUT_DIR/responses/exact-pong.json")" grep -Eq '"id":"msg_01bdrk[A-Za-z0-9]{18}"' "$OUT_DIR/responses/exact-pong.json"
assert_eq exact_json_text "$(jq -r '.content[0].text // ""' "$OUT_DIR/responses/exact-json.json")" '{"alpha":1,"beta":"two"}'
assert_eq exact_json_output_tokens "$(jq -r '.usage.output_tokens // -1' "$OUT_DIR/responses/exact-json.json")" 18
assert_eq tool_stop_reason "$(jq -r '.stop_reason // ""' "$OUT_DIR/responses/tool.json")" tool_use
assert_true tool_id "bedrock tool id" grep -Eq '"id":"toolu_bdrk_' "$OUT_DIR/responses/tool.json"
assert_true stream_metrics "amazon-bedrock-invocationMetrics" grep -q 'amazon-bedrock-invocationMetrics' "$OUT_DIR/responses/exact-pong-stream.sse"
assert_true stream_final_usage "output_tokens=4" grep -Eq '"output_tokens":4' "$OUT_DIR/responses/exact-pong-stream.sse"
cache_first_create="$(jq -r '.usage.cache_creation_input_tokens // 0' "$OUT_DIR/responses/cache-create.json")"
cache_first_read="$(jq -r '.usage.cache_read_input_tokens // 0' "$OUT_DIR/responses/cache-create.json")"
cache_second_read="$(jq -r '.usage.cache_read_input_tokens // 0' "$OUT_DIR/responses/cache-read.json")"
cache_first_total=$((cache_first_create + cache_first_read))
assert_eq cache_first_create_or_read_matches_second_read "$cache_first_total" "$cache_second_read"
assert_true cache_read_nonzero "$cache_second_read" test "$cache_second_read" -gt 0
assert_eq image_text "$(jq -r '.content[0].text // ""' "$OUT_DIR/responses/image.json")" Red
assert_eq spatial_text "$(jq -r '.content[0].text // ""' "$OUT_DIR/responses/spatial.json")" Red
assert_eq identity_backend "$(jq -r '.content[0].text | fromjson | .backend' "$OUT_DIR/responses/identity.json" 2>/dev/null || true)" unknown
assert_eq identity_runtime "$(jq -r '.content[0].text | fromjson | .runtime_product' "$OUT_DIR/responses/identity.json" 2>/dev/null || true)" unknown

concurrent_ok=0
for i in $(seq 1 10); do
  if [[ "$(jq -r '.content[0].text // ""' "$OUT_DIR/responses/concurrent/$i.json")" == pong ]] && grep -q $'\t200\t' "$OUT_DIR/responses/concurrent/$i.meta"; then
    concurrent_ok=$((concurrent_ok + 1))
  fi
done
assert_eq concurrent_exact_pong "$concurrent_ok" 10

printf 'out_dir=%s\n' "$OUT_DIR"
column -t -s $'\t' "$RESULTS" || true
printf '\n'
column -t -s $'\t' "$ASSERTIONS" || true

assertion_failures="$(awk -F '\t' 'NR > 1 && $2 == "FAIL" { count++ } END { print count + 0 }' "$ASSERTIONS")"
if [[ "$assertion_failures" -gt 0 ]]; then
  printf '\n%s assertion(s) failed\n' "$assertion_failures" >&2
  exit 1
fi
