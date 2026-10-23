# Design Philosophy

A handful of convictions shaped wsPrism. They are worth knowing before you read the rest of the book, because most of the behaviour described later follows from them.

## Keep the sockets out of the application

A persistent connection is a long-lived piece of state. It has to be pinged, timed out, rate limited, authenticated again when its token expires, and woken whenever something happens in any room it belongs to. Application servers are good at short, independent requests; they are rarely good at holding a few hundred thousand of these at once.

wsPrism takes that work off the application. It runs as its own process next to your backend, a sidecar rather than a library. Your services remain stateless HTTP services that can be deployed, scaled and restarted on their own schedule. They talk to the gateway in three narrow ways:

- they decide who may connect (one-time tickets, JWTs, or a connect proxy the gateway calls);
- they push to rooms and disconnect users through the server API;
- they answer the requests you route to them through the RPC proxy.

Nothing in that list requires Rust, and nothing in it requires your backend to know which gateway node a user is on.

## Two lanes, because data comes in two kinds

Realtime traffic is not uniform. A chat message must arrive, exactly once and in order. The position of a player who moves sixty times a second does not need to arrive at all if a newer position is already on its way; delivering the stale one only adds latency.

Most systems pick one of these models and make the other awkward. wsPrism carries both on one connection:

- The **Ext lane** carries JSON envelopes. It is reliable: messages are delivered in order, or the client is told they could not be.
- The **Hot lane** carries small binary frames, routed by a service id and an opcode. It is lossy by design. When a recipient falls behind, the gateway keeps only the latest value for each key instead of queueing every intermediate one.

Choosing the lane is choosing the delivery guarantee, and the choice is explicit in the protocol rather than hidden in a configuration flag.

## Deny by default

A gateway sits on the public internet in front of your users. Its defaults should be the safe ones, and loosening them should be a deliberate act you can see in a configuration review.

So a tenant accepts only the message types on its allowlist. Browser origins are refused unless listed. Tickets are not read from the query string, where proxies would log them. Every limit (frame size, rooms per user, sessions per user, request rate) has a finite default. The configuration parser rejects fields it does not know, so a typo cannot silently turn a protection off.

## Fail loudly, never silently

Distributed systems lose messages. What matters is whether anyone finds out.

wsPrism treats a silent loss as a defect. When a cluster node briefly loses its Redis subscription, the members of the affected rooms receive `sys.gap` and know to reload. When history cannot be replayed, the reply says `recovered: false`. When Redis is too slow to accept more work, publishers get `UNAVAILABLE` immediately rather than a success that turns into a loss a minute later. When a node discovers that another node has taken its place, it stops instead of writing state it no longer owns.

The [cluster chapter](guide/cluster.md) lists every failure we test and what clients see in each case.

## Measure before claiming

Every performance statement in this book points to a measurement, with the scenario, the hardware and the raw results in the repository. When an optimisation did not pay for itself, it was reverted and the result was written down anyway. When a comparison with another product is unfair in some respect (same machine, default settings for the competitor), the benchmark notes say so.

This also applies to correctness. Failure behaviour is described only after a chaos or soak test exercises it, and the remaining blind spots are listed in [Security](project/security.md) instead of being left out.

## Boundaries before features

New capabilities go behind a boundary first: a trait, a registry entry, a configuration section. Authentication is a chain of authenticators, and the built-in ones use the same interface as yours. Room traffic between nodes goes through a broker trait, and the single-node and Redis brokers are two implementations of it. Services are registered by name, and the built-in `room` and `chat` services have no special path through the core.

The result is that most customisation needs no fork. You add an authenticator, a service or a backend route; you do not patch the dispatcher.

## Contracts only grow

Clients update slowly. A mobile app can stay in use for a year after its last release. So the wire protocol, the server API and the configuration format only grow: new fields, codes and endpoints may appear, but existing ones keep their names and meanings. A change that cannot be made that way gets a new protocol version negotiated alongside the old one, as v2 was next to v1. The Rust API is allowed to move faster while the project is at 0.x, and [Rust API](reference/rust-api.md) says exactly which parts are stable.
