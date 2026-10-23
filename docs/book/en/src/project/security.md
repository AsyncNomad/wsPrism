# Security

wsPrism sits in front of your users. It authenticates their connections, enforces each tenant's policy and carries their messages, so its defects tend to be security defects. This page says how to report one, what is already in place, and what has not been verified yet.

## Reporting a vulnerability

Please do not open a public issue. Report privately through GitHub's advisory form:

<https://github.com/AsyncNomad/wsPrism/security/advisories/new>

Include what you can: the affected version or commit, the configuration involved (without secrets), what an attacker can do and what they need (an authenticated user, a tenant API key, a network position), and steps to reproduce.

The project is young and maintained by a small team, so these are goals rather than contractual terms: an acknowledgement within seven days, an assessment and a fix or mitigation plan once the issue is reproduced, and a release with a GitHub security advisory that credits you unless you prefer otherwise. Please allow reasonable time for the fix before disclosing.

Until 1.0, security fixes go into the latest 0.x release only.

## Scope

In scope: the crates in the repository, the gateway binary and container image, and the JavaScript SDK. Examples: bypassing authentication or tenant isolation, reaching another tenant's rooms or data, crashes or unbounded resource use triggered by a client, secrets in logs or responses.

Out of scope: benchmark tools and scripts, throwaway experiments under `spikes/`, and problems that require a configuration the documentation warns against (the development authenticator in production, the server API on a public interface).

## What is in place

**Authentication.** JWT algorithms are pinned per authenticator; configured issuer and audience are mandatory; JWKS refetches are rate limited, symmetric keys from URLs are ignored, and stale keys stop being trusted. One-time tickets are single-use (even on failure), bound to a tenant, short-lived, capped in number, and stored only as hashes in Redis. In-band refresh cannot switch users and applies narrowed permissions immediately.

**Authorisation.** Every join checks the subscribe permission, every publish through a service context checks the publish permission, and Hot frames reach only rooms the sender has joined. Allowlists are deny-by-default.

**Input.** Frame size limits are enforced per tenant in the WebSocket codec. Decoders have no panicking paths and are fuzzed. Production code contains no `unwrap`, `expect` or `panic!`, enforced by lints, and no slice indexing that can panic. User, tenant and session ids are restricted to a safe alphabet because they become parts of keys; room names cannot collide with other keys.

**Network.** Browser origins are checked. Client addresses behind proxies use the rightmost untrusted `X-Forwarded-For` hop. Handshake rate limits are bounded in memory. The backend HTTP client never follows redirects, ignores proxy environment variables, caps response size and masks credentials in logs.

**Time.** Idle, ping and credential expiry use a monotonic clock, so a wall clock that jumps backwards (which happens on some virtual machines) cannot keep a session alive or close it early.

**Supply chain.** Dependencies are audited in CI with `cargo deny`.

## How defects were found

The project keeps a log of every defect fixed, how it was found and the test that reproduces it: [`docs/HARDENING.md`](https://github.com/AsyncNomad/wsPrism/blob/main/docs/HARDENING.md). The sources so far:

- **Reviews** before each milestone, including a full review before the first release.
- **Fuzzing** of every parser that sees untrusted input.
- **Chaos tests** that kill nodes, restart Redis, partition the network and perform rolling updates.
- **A randomised soak test** with a judge that accounts for every message. It found, among other things, a history read that failed on a dead Redis socket after a restart, and harness artefacts that would have hidden real losses.
- **A pre-release defect hunt** that read the code in order of risk and confirmed each suspicion with a failing test before fixing it. It found five defects: concurrent connections with the same session id could all survive, on one node and across the cluster; a server API Hot publish did not reach members on other nodes; a token with an expiry far in the future could panic the handshake; and the SDK could permanently drop a room whose rejoin was interrupted.

## Known blind spots

- No external security audit has been performed.
- Fencing covers room traffic. A fenced node's session and membership writes are cleaned up after it stops rather than refused.
- The soak test injects network faults between nodes and Redis (delay, blackhole, reset), but not packet loss, reordering or asymmetric partitions, and not between clients and nodes.
- Sentinel failover is tested separately, not inside the soak.
- Tenants are not fully isolated under extreme load: no tenant starves, but one tenant's load raises another's latency. Use per-tenant rate limits.
