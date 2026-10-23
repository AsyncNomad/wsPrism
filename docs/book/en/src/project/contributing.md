# Contributing

Contributions are welcome: bug reports, documentation fixes, tests, and code. This page explains how the project works so that your change fits in.

## Before you start

For anything beyond a small fix, open an issue first and describe the problem you want to solve. Many design questions have already been argued in the [decision records](../architecture/decisions.md), and a short discussion can save you a rewrite.

Security problems go through the private channel described in [Security](security.md), not the issue tracker.

## Building and testing

```bash
cargo build --workspace
cargo test --workspace --locked
```

Integration tests start the real gateway. Cluster tests need a local Redis:

```bash
docker run -d --name wsp-redis -p 6379:6379 redis:7.2 redis-server --save "" --appendonly no
```

The SDK and the example application have their own end-to-end tests:

```bash
sdk/js/run-tests.sh
examples/chat-and-state/run-tests.sh
```

CI runs all of this plus formatting, Clippy with warnings as errors, a build on the minimum supported Rust version, the dependency audit, the chaos tests and a short fuzzing run. Run at least these before opening a pull request:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

## How changes are made

- **Fixes start with a failing test.** Write the test that reproduces the defect, check that it fails, then fix it. Remove the fix once to confirm the test catches it.
- **One change per commit.** Do not mix refactoring with behaviour changes. Commit messages use conventional prefixes (`feat:`, `fix:`, `docs:`, `perf:`, `test:`, `refactor:`).
- **Boundaries before features.** New capabilities go behind a trait, a registry or a configuration section. A special case in the core path (`if svc == "x"`) needs a very good reason and a note saying how it will be removed.
- **Measure performance changes.** Any change that claims to be faster comes with before-and-after numbers from the benchmark scenarios. A change that does not pay for itself is reverted, and the result is still written down.
- **Write down decisions.** Client-visible changes go into the protocol documentation, design decisions and the alternatives you rejected into an ADR, and defect fixes into the hardening log.
- **Contracts only grow.** Changes to the wire protocol, the server API or the configuration format must be additive. A new configuration field always comes with a default.

## Flaky tests

A test that fails intermittently is treated as a defect, in the test or in the product. Integration tests are run repeatedly to check that they are stable, and flakiness is investigated rather than retried away.

## License

By contributing, you agree that your contributions are licensed under the Apache License 2.0, like the rest of the project.
