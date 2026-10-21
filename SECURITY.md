# Security Policy

wsPrism is a gateway in front of your users: it authenticates connections,
enforces tenant policies and carries their messages. We take reports about
it seriously.

## Reporting a vulnerability

**Please do not open a public issue.** Report privately through GitHub:
<https://github.com/AsyncNomad/wsPrism/security/advisories/new>
("Report a vulnerability" on the repository's Security tab).

Include what you can of:

- the affected version or commit, and the configuration involved (remove
  secrets);
- what an attacker can do, and what they need (an authenticated user, a
  tenant API key, network position, ...);
- steps or a proof of concept to reproduce it.

## What to expect

This is a young project maintained by a small team; these are goals, not
contractual terms:

- an acknowledgement within 7 days;
- an assessment, and a fix or mitigation plan, as soon as we have
  reproduced the issue;
- a fixed release and a GitHub security advisory, crediting you unless you
  prefer otherwise. Please give us a reasonable time to ship the fix before
  disclosing it.

## Supported versions

Until 1.0, security fixes go into the latest 0.x release only. Upgrade to
it to receive them (docs/API.md describes what may change between 0.x
releases).

## Scope

In scope: the crates in this repository (`wsprism`, `wsprism-gateway`,
`wsprism-engine`, `wsprism-protocol`, `wsprism-redis`), the gateway binary
and container image built from it, and the JavaScript SDK (`sdk/js`).
Examples include authentication or tenant-isolation bypasses, a client
reaching another tenant's rooms or data, crashes or unbounded resource use
triggered by a client, and secrets showing up in logs or responses.

Out of scope: the benchmark tools and scripts (`bench/`,
`wsprism-loadgen`), the throwaway experiments in `spikes/`, and issues that
need a misconfiguration the documentation warns against (for example the
development authenticator in production, or the server API on a public
interface).

## Hardening already in place

The defects fixed so far and how they were found (reviews, fuzzing, chaos
tests) are recorded in [docs/HARDENING.md](docs/HARDENING.md). Dependencies
are audited in CI (`cargo deny`), and the parsers that see untrusted input
are fuzzed.
