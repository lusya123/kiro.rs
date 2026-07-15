# POMO D7 direct replay

This replay uses the POMO report's D7 nonce (`84aa4d5a`) and the same tool,
schema, forced-tool mode, model, prompt, and stream settings as the detector
probe. The transient API key was used only for the request and is not stored.

- HTTP status: `200`
- Tool name: `get_weather`
- Stop reason: `tool_use`
- JSON delta count: `10`

`d7-validation.json` reconstructs the deltas into:

```json
{"city": "Exampleville-84aa4d5a", "unit": "celsius"}
```

The JSON is valid and exactly matches the requested schema and values. The
fresh ZTest report nevertheless records `raw_arguments={}` and gives POMO the
same D7 score (`60`) as Q2. This independently confirms a detector aggregation
issue rather than a Q2-specific tool-stream defect.
