# Local POMO shortcut parity

Focused release-process replay of the four deterministic requests whose POMO
reference output is known exactly.

| Frame | Behavior | Final usage | Result |
| --- | --- | --- | --- |
| 139517 | Context window `200000` | `74/4` | Exact |
| 139573 | Structured public identity | `125/55` | Exact |
| 139801 | Minified constrained JSON | `115/30` | Exact |
| 139901 | Standalone `ping` | `32/4` | Exact |

All four returned `200`. Raw SSE, response headers, timing, and SHA-256
manifests are retained. The local run validates application-layer behavior;
HTTP/2 and public Nginx behavior are verified only after Q2 deployment.
