# Design Philosophy

wsPrism was built to solve the specific challenges of scaling realtime applications. Here are the core principles that guide its development.

## 1. The "Sidecar" Architecture

wsPrism is not just a library; it is a **Realtime Edge Gateway**.

In a traditional setup, your monolithic backend handles everything: database queries, business logic, and maintaining thousands of idle WebSocket connections. This scales poorly because stateful connections consume resources even when idle.

wsPrism separates concerns:
- It handles the **"connection storm"** (handshakes, heartbeats, ping/pong).
- It manages thousands of **concurrent stateful sessions**.
- It routes filtered, valid messages to your services.

**Result:** Your main backend logic remains stateless and scalable (like standard REST APIs), while wsPrism guarantees low-latency delivery.

## 2. Rust Performance, Human Accessibility

We utilize **Rust** for the Core Engine to guarantee memory safety, zero-cost abstractions, and stable tail latencies. This is critical for high-frequency trading or gaming where a garbage collector pause is unacceptable.

However, wsPrism is designed as a **Platform**, not just a framework for Rust developers:
- **Configurable Policies:** Rate limits, packet sizes, and allowlists are managed via simple YAML configurations.
- **Ops-Ready:** Built-in metrics (Prometheus) and structured logging for enterprise observability.
- **Extensible:** (Future roadmap) Write plugins in scripting languages or WASM.

## 3. The "Dual Lane" Protocol

Most frameworks force you to choose between "Easy JSON" (Socket.io) or "Fast Binary" (gRPC / Raw UDP). wsPrism unifies them into a single connection.

### A. Ext Lane (Text / JSON)
- **Use Case:** Chat, lobby logic, room management, complex data structures.
- **Priority:** Reliable delivery.
- **Format:** Human-readable JSON. Easy to debug and develop.

### B. Hot Lane (Binary)
- **Use Case:** Player movement, physics synchronization, market ticks, high-frequency signals.
- **Priority:** Latency critical (Lossy / Latest-state-wins).
- **Format:** Compact Binary. Zero-allocation routing, minimal overhead, routed strictly by Service ID and OpCode.
