# Q2 deployed aff5e6 live suite

Production-like public E2E evidence for `https://q2.quietfox.sbs` while it ran
the immutable `aws-b-beta-aff5e6` image.

- 20 protocol and behavior assertions passed.
- Authentication, model errors, malformed input, exact text/JSON, thinking,
  tools, cache accounting, image input, identity cleaning, and Bedrock IDs
  were exercised through the public HTTPS endpoint.
- Ten concurrent exact-response requests all returned `200` with `pong`.
- AWS-B intentionally exposes no public `/v1/messages/count_tokens` endpoint;
  its expected result is `404`.

`results.tsv` contains transport results and `assertions.tsv` contains the
evaluated checks. Request and response bodies are retained verbatim.
