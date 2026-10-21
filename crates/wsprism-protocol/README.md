# wsprism-protocol

The wsPrism wire contract, independent of transport and runtime: JSON
envelopes (Ext lane), binary Hot frames (v1 and v2 codecs), the v2 close-code
mapping, and client error codes. Use it to write a client or tool that speaks
the protocol; servers and services use [`wsprism`](https://crates.io/crates/wsprism).

Specification: [docs/PROTOCOL.md](https://github.com/AsyncNomad/wsPrism/blob/main/docs/PROTOCOL.md). Parsers are panic-free
and fuzzed.

Minimum supported Rust version: 1.95. License: Apache-2.0.
