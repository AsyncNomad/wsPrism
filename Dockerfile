# syntax=docker/dockerfile:1
# wsPrism gateway image: a static-ish binary on distroless (glibc + CA
# certificates, no shell), running as an unprivileged user.
#
#   docker build -t wsprism-gateway .
#   docker run -v $PWD/wsprism.yaml:/etc/wsprism/wsprism.yaml:ro -p 8080:8080 wsprism-gateway
#
# The configuration is not baked in: mount it at /etc/wsprism/wsprism.yaml
# (or point WSPRISM_CONFIG elsewhere). Values can come from the environment
# (${VAR} in the file, docs/CONFIG.md). Probes: GET /healthz, /readyz.
# Example cluster: deploy/compose/.

ARG RUST_VERSION=1.95

FROM rust:${RUST_VERSION}-bookworm AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p wsprism-gateway \
 && cp target/release/wsprism-gateway /wsprism-gateway

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /wsprism-gateway /usr/local/bin/wsprism-gateway
ENV WSPRISM_CONFIG=/etc/wsprism/wsprism.yaml \
    WSPRISM_LOG_FORMAT=json \
    RUST_LOG=info
# WebSocket (gateway.listen) and server API / metrics (gateway.api.listen),
# as in deploy/compose/wsprism.yaml.
EXPOSE 8080 8081
USER nonroot
# SIGTERM drains (docs/OPERATIONS.md): give it more than drain_grace_ms.
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/local/bin/wsprism-gateway"]
