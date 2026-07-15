# Q2 D7 packet-capture evidence

This directory preserves the exact D7 request body and SSE response body from
the completed Q2 ZTest run.

- Remote rolling capture: `/tmp/qcap/q.pcap02`
- Q2 container address at capture time: `172.17.0.2:8990`
- TCP stream: `187`
- Request frame: `6475`
- Request time: epoch `1784134714.065959`
- Request body size: `891` bytes

Only the HTTP body and response headers were extracted. Authentication headers,
the source pcap, SSH credentials, and API keys are not stored here.

Files:

- `d7-request.raw.json`: exact compact request body received by Q2.
- `d7-response.http1-chunked.raw`: reconstructed internal HTTP/1.1 response;
  the SSH text bridge normalized CRLF framing to LF.
- `d7-response.headers`: response headers after that newline normalization.
- `d7-response.sse`: decoded SSE body.
- `d7-validation.json`: structured reconstruction of every
  `input_json_delta`.

The validation reconstructs four deltas into:

```json
{"city": "Exampleville-8aaf1f96", "unit": "celsius"}
```

The JSON is valid and exactly matches the requested schema and values. ZTest's
reported `raw_arguments={}` therefore does not describe the bytes Q2 returned.
