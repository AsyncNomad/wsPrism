# Releasing

All five crates are released together with one version (ADR 0009 §5-2).
Publish order follows the dependency chain.

## Checklist

1. **Branch:** release from `main` with a clean tree.
2. **Version:** set `version` in `[workspace.package]` (root `Cargo.toml`) and
   in `[workspace.dependencies]` for the four internal library crates
   (`wsprism-protocol`, `-engine`, `-redis`, `-gateway`).
3. **Changelog:** rename `[Unreleased]` in `CHANGELOG.md` to the version and
   date. Every breaking Rust API change and every MSRV change must be listed.
4. **Gates** (the same as CI, `.github/workflows/ci.yml`):
   ```sh
   cargo fmt --all --check
   cargo clippy --workspace --all-targets --locked -- -D warnings
   cargo clippy --workspace --lib --locked -- -D warnings
   cargo test --workspace --locked
   WSPRISM_TEST_BROKER=queued cargo test -p wsprism-gateway --locked
   cargo +1.95 check --workspace --all-targets --locked   # MSRV
   cargo deny check                                        # advisories, licenses
   npx --yes -p typescript@5 tsc -p sdk/js                 # SDK type declarations
   # Redis first: with WSPRISM_TEST_REDIS set the SDK suite also runs the
   # chaos tests (without it they are skipped, silently).
   export WSPRISM_TEST_REDIS=$(bench/redis-test.sh)
   cargo test -p wsprism-gateway --test cluster --test cluster_gap --test history
   cargo test -p wsprism-redis
   sdk/js/run-tests.sh && examples/chat-and-state/run-tests.sh
   bench/redis-test.sh stop
   (cd fuzz && for t in hot_frame envelope client_ip origin subprotocol; do
      cargo +nightly fuzz run --target x86_64-unknown-linux-gnu $t -- -max_total_time=60; done)
   docker build -t wsprism-gateway .                       # the image builds
   ```
5. **Package dry run** (builds each packaged crate against the others), in a
   **fresh target directory**: packages of the same version are cached as
   registry crates (immutable to Cargo), so a repeated dry run can verify
   against stale ones and fail for no real reason:
   ```sh
   CARGO_TARGET_DIR=$(mktemp -d) cargo package --workspace --exclude wsprism-loadgen --locked
   ```
6. **Names:** confirm the names are still free or owned by us
   (`https://crates.io/api/v1/crates/<name>`; send no personal data in the
   request). crates.io has no reservations: check right before publishing.
7. **Publish** (needs a crates.io token of an owner):
   ```sh
   cargo publish -p wsprism-protocol
   cargo publish -p wsprism-engine
   cargo publish -p wsprism-redis
   cargo publish -p wsprism-gateway
   cargo publish -p wsprism
   ```
   A published version cannot be deleted, only yanked. If a later crate fails,
   fix and publish the remaining ones with the same version; never republish
   an existing version.
8. **Tag:** `git tag v<version>` and push the tag; create the GitHub release
   from the changelog section.
9. **After the first release:** add `cargo-semver-checks` to CI
   (ADR 0009 §5-6), and set crates.io owners (a team, not only one person).

Not published: `wsprism-loadgen`, `fuzz`, `spikes/*` (`publish = false`).
The JS SDK (`sdk/js`, `"private": true`) is not on npm yet.
