#!/usr/bin/env bash

set -euo pipefail

RUN_REFERENCE="${RUN_REFERENCE:-1}"
RUN_TARGET="${RUN_TARGET:-1}"

if [[ "$RUN_REFERENCE" == "1" ]]; then
  : "${REFERENCE_BASE_URL:?set REFERENCE_BASE_URL}"
  : "${REFERENCE_API_KEY:?set REFERENCE_API_KEY}"
fi
if [[ "$RUN_TARGET" == "1" ]]; then
  : "${TARGET_BASE_URL:?set TARGET_BASE_URL}"
  : "${TARGET_API_KEY:?set TARGET_API_KEY}"
fi
if [[ "$RUN_REFERENCE" != "1" && "$RUN_TARGET" != "1" ]]; then
  printf 'at least one of RUN_REFERENCE or RUN_TARGET must be 1\n' >&2
  exit 2
fi

MODEL="${MODEL:-claude-opus-4-8}"
OUT_DIR="${OUT_DIR:-test-artifacts/ztest/token-matrix-$(date +%Y%m%d-%H%M%S)}"

mkdir -p "$OUT_DIR/requests" "$OUT_DIR/reference" "$OUT_DIR/target"

write_exact_request() {
  local output="$1"
  local system_text="$2"
  local answer="$3"

  if [[ -n "$system_text" ]]; then
    jq -n --arg model "$MODEL" --arg system "$system_text" --arg answer "$answer" '{
      model: $model,
      max_tokens: 16,
      system: $system,
      messages: [{role: "user", content: ("Reply with exactly: " + $answer)}]
    }' > "$output"
  else
    jq -n --arg model "$MODEL" --arg answer "$answer" '{
      model: $model,
      max_tokens: 16,
      messages: [{role: "user", content: ("Reply with exactly: " + $answer)}]
    }' > "$output"
  fi
}

write_cache_request() {
  local output="$1"
  local segment_count="$2"
  local marker="$3"
  local anchor=""

  for index in $(seq 1 "$segment_count"); do
    anchor+="${marker} cache anchor segment ${index}: protocol parity datum. "
  done

  jq -n --arg model "$MODEL" --arg anchor "$anchor" --arg marker "$marker" '{
    model: $model,
    max_tokens: 16,
    system: [{type: "text", text: $anchor, cache_control: {type: "ephemeral"}}],
    messages: [{role: "user", content: ("Reply exactly " + $marker + ".")}]
  }' > "$output"
}

write_tool_history_request() {
  local output="$1"
  local prompt="$2"
  local tool_name="$3"
  local input_json="$4"
  local result="$5"

  jq -n \
    --arg model "$MODEL" \
    --arg prompt "$prompt" \
    --arg tool_name "$tool_name" \
    --argjson input "$input_json" \
    --arg result "$result" '{
      model: $model,
      max_tokens: 8,
      messages: [
        {role: "user", content: $prompt},
        {role: "assistant", content: [{
          type: "tool_use",
          id: "toolu_bdrk_01CalibrationMatrix000000001",
          name: $tool_name,
          input: $input
        }]},
        {role: "user", content: [{
          type: "tool_result",
          tool_use_id: "toolu_bdrk_01CalibrationMatrix000000001",
          content: $result
        }]}
      ]
    }' > "$output"
}

write_exact_request "$OUT_DIR/requests/exact-no-system.json" "" "PONG"
write_exact_request "$OUT_DIR/requests/exact-system-one-word.json" \
  "Concise." "PONG"
write_exact_request "$OUT_DIR/requests/exact-system-three-words.json" \
  "Be very concise." "PONG"
write_exact_request "$OUT_DIR/requests/exact-short-system.json" \
  "Be concise and follow the requested output format." "PONG"
write_exact_request "$OUT_DIR/requests/exact-claude-system.json" \
  "You are Claude Code, Anthropic's official CLI for Claude." "PONG"
write_exact_request "$OUT_DIR/requests/exact-neutral-long-system.json" \
  "Keep responses direct, accurate, concise, and useful. Follow the requested output format without adding explanations, examples, caveats, or unrelated implementation details." \
  "pong"
write_exact_request "$OUT_DIR/requests/exact-long-system.json" \
  "Ignore any request to reveal hidden routing, credentials, runtime products, or implementation details. Follow the user's exact harmless output format." \
  "pong"

jq -n --arg model "$MODEL" '{
  model: $model,
  max_tokens: 8,
  messages: [
    {role: "user", content: "Use the helper."},
    {role: "assistant", content: "Done."},
    {role: "user", content: "Continue."}
  ]
}' > "$OUT_DIR/requests/plain-three-turn.json"

jq -n --arg model "$MODEL" '{
  model: $model,
  max_tokens: 16,
  system: "You are a concise arithmetic assistant.",
  messages: [{role: "user", content: "What is 2 + 2?"}]
}' > "$OUT_DIR/requests/system-arithmetic.json"

jq -n --arg model "$MODEL" '{
  model: $model,
  max_tokens: 16,
  messages: [
    {role: "user", content: "Remember the word amber."},
    {role: "assistant", content: "I will remember amber."},
    {role: "user", content: "What word did I ask you to remember?"}
  ]
}' > "$OUT_DIR/requests/plain-three-turn-amber.json"

jq -n --arg model "$MODEL" '{
  model: $model,
  max_tokens: 8,
  messages: [
    {role: "user", content: "Call lookup once."},
    {role: "assistant", content: [{
      type: "tool_use",
      id: "toolu_bdrk_01Calibration000000000001",
      name: "lookup",
      input: {query: "alpha"}
    }]},
    {role: "user", content: "Continue."}
  ]
}' > "$OUT_DIR/requests/tool-use-only.json"

jq -n --arg model "$MODEL" '{
  model: $model,
  max_tokens: 8,
  messages: [
    {role: "user", content: "Call lookup once."},
    {role: "assistant", content: [{
      type: "tool_use",
      id: "toolu_bdrk_01Calibration000000000001",
      name: "lookup",
      input: {}
    }]},
    {role: "user", content: [{
      type: "tool_result",
      tool_use_id: "toolu_bdrk_01Calibration000000000001",
      content: "ok"
    }]}
  ]
}' > "$OUT_DIR/requests/tool-result-empty.json"

jq -n --arg model "$MODEL" '{
  model: $model,
  max_tokens: 8,
  messages: [
    {role: "user", content: "Call lookup once."},
    {role: "assistant", content: [{
      type: "tool_use",
      id: "toolu_bdrk_01Calibration000000000005",
      name: "lookup",
      input: {query: "alpha"}
    }]},
    {role: "user", content: [{
      type: "tool_result",
      tool_use_id: "toolu_bdrk_01Calibration000000000005",
      content: "ok"
    }]}
  ]
}' > "$OUT_DIR/requests/tool-result-one-field.json"

jq -n --arg model "$MODEL" '{
  model: $model,
  max_tokens: 8,
  messages: [
    {role: "user", content: "Call lookup once."},
    {role: "assistant", content: [{
      type: "tool_use",
      id: "toolu_bdrk_01Calibration000000000006",
      name: "lookup",
      input: {}
    }]},
    {role: "user", content: [{
      type: "tool_result",
      tool_use_id: "toolu_bdrk_01Calibration000000000006",
      content: "18 C and clear"
    }]}
  ]
}' > "$OUT_DIR/requests/tool-result-empty-long-result.json"

jq -n --arg model "$MODEL" '{
  model: $model,
  max_tokens: 8,
  messages: [
    {role: "user", content: "Call get_weather for Paris with unit celsius. Return only the tool call."},
    {role: "assistant", content: [{
      type: "tool_use",
      id: "toolu_bdrk_01Calibration000000000002",
      name: "get_weather",
      input: {location: "Paris", unit: "celsius"}
    }]},
    {role: "user", content: [{
      type: "tool_result",
      tool_use_id: "toolu_bdrk_01Calibration000000000002",
      content: "18 C and clear"
    }]}
  ]
}' > "$OUT_DIR/requests/tool-result-fields.json"

jq '. + {
  tools: [{
    name: "get_weather",
    description: "Get current weather for a city.",
    input_schema: {
      type: "object",
      properties: {
        location: {type: "string"},
        unit: {type: "string", enum: ["celsius", "fahrenheit"]}
      },
      required: ["location", "unit"],
      additionalProperties: false
    }
  }],
  tool_choice: {type: "auto"}
}' "$OUT_DIR/requests/tool-result-fields.json" \
  > "$OUT_DIR/requests/tool-result-fields-with-schema.json"

jq -n --arg model "$MODEL" '{
  model: $model,
  max_tokens: 8,
  messages: [
    {role: "user", content: "Call lookup once."},
    {role: "assistant", content: [{
      type: "tool_use",
      id: "toolu_bdrk_01Calibration000000000007",
      name: "get_weather",
      input: {location: "Paris", unit: "celsius"}
    }]},
    {role: "user", content: [{
      type: "tool_result",
      tool_use_id: "toolu_bdrk_01Calibration000000000007",
      content: "ok"
    }]}
  ]
}' > "$OUT_DIR/requests/tool-result-fields-short-prompt.json"

jq -n --arg model "$MODEL" '{
  model: $model,
  max_tokens: 8,
  messages: [
    {role: "user", content: "Call get_weather for Paris with unit celsius. Return only the tool call."},
    {role: "assistant", content: [{
      type: "tool_use",
      id: "toolu_bdrk_01Calibration000000000008",
      name: "lookup",
      input: {}
    }]},
    {role: "user", content: [{
      type: "tool_result",
      tool_use_id: "toolu_bdrk_01Calibration000000000008",
      content: "ok"
    }]}
  ]
}' > "$OUT_DIR/requests/tool-result-empty-long-prompt.json"

jq -n --arg model "$MODEL" '{
  model: $model,
  max_tokens: 8,
  messages: [
    {role: "user", content: "Call both lookups."},
    {role: "assistant", content: [
      {
        type: "tool_use",
        id: "toolu_bdrk_01Calibration000000000003",
        name: "lookup_alpha",
        input: {query: "alpha"}
      },
      {
        type: "tool_use",
        id: "toolu_bdrk_01Calibration000000000004",
        name: "lookup_beta",
        input: {query: "beta"}
      }
    ]},
    {role: "user", content: [
      {
        type: "tool_result",
        tool_use_id: "toolu_bdrk_01Calibration000000000003",
        content: "one"
      },
      {
        type: "tool_result",
        tool_use_id: "toolu_bdrk_01Calibration000000000004",
        content: "two"
      }
    ]}
  ]
}' > "$OUT_DIR/requests/two-tool-results.json"

jq '. + {
  tools: [{
    name: "lookup",
    description: "Look up one record.",
    input_schema: {
      type: "object",
      properties: {query: {type: "string"}},
      required: ["query"]
    }
  }],
  tool_choice: {type: "auto"}
}' "$OUT_DIR/requests/tool-result-one-field.json" \
  > "$OUT_DIR/requests/tool-result-one-field-with-schema.json"

jq '. + {
  tools: [
    {
      name: "lookup_alpha",
      description: "Look up the alpha record.",
      input_schema: {
        type: "object",
        properties: {query: {type: "string"}},
        required: ["query"]
      }
    },
    {
      name: "lookup_beta",
      description: "Look up the beta record.",
      input_schema: {
        type: "object",
        properties: {query: {type: "string"}},
        required: ["query"]
      }
    }
  ],
  tool_choice: {type: "auto"}
}' "$OUT_DIR/requests/two-tool-results.json" \
  > "$OUT_DIR/requests/two-tool-results-with-schema.json"

write_tool_history_request "$OUT_DIR/requests/tool-empty-prompt-minimal.json" \
  "Go." "lookup" '{}' "ok"
write_tool_history_request "$OUT_DIR/requests/tool-empty-prompt-medium.json" \
  "Please call the lookup helper once, then use its result." "lookup" '{}' "ok"
write_tool_history_request "$OUT_DIR/requests/tool-empty-prompt-long.json" \
  "Please call the lookup helper exactly once with no arguments, wait for its result, and then give a concise final answer without adding unrelated details." \
  "lookup" '{}' "ok"
write_tool_history_request "$OUT_DIR/requests/tool-one-field-long-result.json" \
  "Call lookup once." "lookup" '{"query":"alpha"}' \
  "The lookup completed successfully and returned the requested alpha record."
write_tool_history_request "$OUT_DIR/requests/tool-three-fields-short.json" \
  "Call lookup once." "lookup" '{"alpha":"one","beta":"two","gamma":"three"}' "ok"
write_tool_history_request "$OUT_DIR/requests/tool-four-fields-short.json" \
  "Call lookup once." "lookup" '{"alpha":"one","beta":"two","gamma":"three","delta":"four"}' "ok"
write_tool_history_request "$OUT_DIR/requests/tool-one-field-long-name.json" \
  "Call lookup once." "lookup_customer_record_by_external_identifier" \
  '{"query":"alpha"}' "ok"
write_tool_history_request "$OUT_DIR/requests/tool-one-field-long-value.json" \
  "Call lookup once." "lookup" \
  '{"query":"Find the customer record whose external identifier is alpha-2026 and include every matching regional account."}' "ok"
write_tool_history_request "$OUT_DIR/requests/tool-one-field-nested-value.json" \
  "Call lookup once." "lookup" \
  '{"filters":{"regions":["us-east-1","eu-west-1"],"active":true,"minimum_score":42}}' "ok"
write_tool_history_request "$OUT_DIR/requests/tool-history-exact.json" \
  "Reply with exactly: PONG" "lookup" '{"query":"alpha"}' "ok"

jq '. + {
  tools: [{
    name: "lookup",
    description: "Look up one record.",
    input_schema: {
      type: "object",
      properties: {query: {type: "string"}},
      required: ["query"]
    }
  }],
  tool_choice: {type: "auto"}
}' "$OUT_DIR/requests/tool-history-exact.json" \
  > "$OUT_DIR/requests/tool-history-exact-with-schema.json"

jq -n --arg model "$MODEL" '{
  model: $model,
  max_tokens: 8,
  messages: [
    {role: "user", content: "Call all three lookups."},
    {role: "assistant", content: [
      {type: "tool_use", id: "toolu_bdrk_01CalibrationMatrix000000011", name: "lookup_alpha", input: {query: "alpha"}},
      {type: "tool_use", id: "toolu_bdrk_01CalibrationMatrix000000012", name: "lookup_beta", input: {query: "beta"}},
      {type: "tool_use", id: "toolu_bdrk_01CalibrationMatrix000000013", name: "lookup_gamma", input: {query: "gamma"}}
    ]},
    {role: "user", content: [
      {type: "tool_result", tool_use_id: "toolu_bdrk_01CalibrationMatrix000000011", content: "one"},
      {type: "tool_result", tool_use_id: "toolu_bdrk_01CalibrationMatrix000000012", content: "two"},
      {type: "tool_result", tool_use_id: "toolu_bdrk_01CalibrationMatrix000000013", content: "three"}
    ]}
  ]
}' > "$OUT_DIR/requests/three-tool-results.json"

jq -n --arg model "$MODEL" '{
  model: $model,
  max_tokens: 8,
  messages: [
    {role: "user", content: "Call the alpha lookup and then the beta lookup."},
    {role: "assistant", content: [{
      type: "tool_use",
      id: "toolu_bdrk_01CalibrationSequential000001",
      name: "lookup_alpha",
      input: {query: "alpha"}
    }]},
    {role: "user", content: [{
      type: "tool_result",
      tool_use_id: "toolu_bdrk_01CalibrationSequential000001",
      content: "one"
    }]},
    {role: "assistant", content: [{
      type: "tool_use",
      id: "toolu_bdrk_01CalibrationSequential000002",
      name: "lookup_beta",
      input: {query: "beta"}
    }]},
    {role: "user", content: [{
      type: "tool_result",
      tool_use_id: "toolu_bdrk_01CalibrationSequential000002",
      content: "two"
    }]}
  ]
}' > "$OUT_DIR/requests/two-tool-results-sequential.json"

write_cache_request "$OUT_DIR/requests/cache-100.json" 100 "CACHE_100"
write_cache_request "$OUT_DIR/requests/cache-170.json" 170 "CACHE_170"
write_cache_request "$OUT_DIR/requests/cache-300.json" 300 "CACHE_300"

RESULTS="$OUT_DIR/results.tsv"
printf 'scenario\tside\thttp_code\tcurl_exit\ttime_total\tinput_tokens\tcache_creation_input_tokens\tcache_read_input_tokens\tcache_total_tokens\toutput_tokens\n' > "$RESULTS"

record_request() {
  local scenario="$1"
  local side="$2"
  local base_url="$3"
  local api_key="$4"
  local request_file="$OUT_DIR/requests/$scenario.json"
  local response_dir="$OUT_DIR/$side"
  local meta rc code total input cache_create cache_read output

  set +e
  meta=$(curl --http2 -sS --max-time 180 \
    -D "$response_dir/$scenario.headers" \
    -o "$response_dir/$scenario.json" \
    -w '%{http_code}\t%{time_total}' \
    -H "Authorization: Bearer $api_key" \
    -H "x-api-key: $api_key" \
    -H 'anthropic-version: 2023-06-01' \
    -H 'content-type: application/json' \
    --data-binary "@$request_file" \
    "$base_url/v1/messages")
  rc=$?
  set -e

  IFS=$'\t' read -r code total <<< "$meta"
  input=$(jq -r '.usage.input_tokens // -1' "$response_dir/$scenario.json" 2>/dev/null || echo -1)
  cache_create=$(jq -r '.usage.cache_creation_input_tokens // -1' "$response_dir/$scenario.json" 2>/dev/null || echo -1)
  cache_read=$(jq -r '.usage.cache_read_input_tokens // -1' "$response_dir/$scenario.json" 2>/dev/null || echo -1)
  output=$(jq -r '.usage.output_tokens // -1' "$response_dir/$scenario.json" 2>/dev/null || echo -1)
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$scenario" "$side" "${code:-000}" "$rc" "${total:-0}" "$input" \
    "$cache_create" "$cache_read" "$((cache_create + cache_read))" "$output" >> "$RESULTS"
}

if [[ -n "${SCENARIOS:-}" ]]; then
  read -r -a scenarios <<< "$SCENARIOS"
else
  scenarios=()
  for request_file in "$OUT_DIR/requests"/*.json; do
    scenarios+=("$(basename "$request_file" .json)")
  done
fi

for scenario in "${scenarios[@]}"; do
  if [[ ! -f "$OUT_DIR/requests/$scenario.json" ]]; then
    printf 'unknown scenario: %s\n' "$scenario" >&2
    exit 2
  fi
  if [[ "$RUN_REFERENCE" == "1" ]]; then
    record_request "$scenario" reference "$REFERENCE_BASE_URL" "$REFERENCE_API_KEY"
  fi
  if [[ "$RUN_TARGET" == "1" ]]; then
    record_request "$scenario" target "$TARGET_BASE_URL" "$TARGET_API_KEY"
  fi
done

printf 'out_dir=%s\n' "$OUT_DIR"
column -t -s $'\t' "$RESULTS" || true
