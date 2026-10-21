# moq-probe (throwaway, validation V6)

Runs the same state-sync scenario as `wsprism-loadgen statesync`, but through a
MoQ relay (`moq-relay-ietf`, Cloudflare's IETF-draft line). AoI, frame and
cost code are the loadgen's own files (`#[path]`), so the metrics are
identical; the JSON output matches, so `bench/analyze-v.py` applies.

Mapping: each participant publishes namespace `r<room>/p<id>`, track `state`;
each update is one datagram object (group = sequence number). Each participant
subscribes to the `state` track of every other member of its room.

Run via `bench/run-moq.sh` (netlab, self-signed cert generated per run).
