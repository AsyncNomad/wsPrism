# Authentication and Permissions

Every connection presents a credential during the WebSocket handshake. The tenant's **authentication chain** decides who it belongs to and what that user may do. Nothing reaches a service until this has succeeded.

## Presenting the credential

The gateway calls whatever the client presents a *ticket*, whether it is a one-time ticket, a JWT or something your own authenticator understands. A tenant lists, in order, where clients may put it (`policy.ticket_sources`):

| Source | How the client sends it | Suits |
|---|---|---|
| `header` | `Authorization: Bearer <ticket>` | Native apps, game engines, servers |
| `subprotocol` | `Sec-WebSocket-Protocol: wsprism.v2.json, wsprism.auth.<ticket>` | Browsers, which cannot set headers on a WebSocket |
| `query` | `?ticket=<ticket>` | Legacy clients only |

The default is `[header, subprotocol]`. The query string is left out on purpose: URLs end up in proxy and access logs, and a credential in a log is a leaked credential. If you do enable it, use it only with one-time tickets, which are worthless once used.

The gateway never echoes the `wsprism.auth.*` entry back in the handshake response.

## The chain

`tenants[].auth` is an ordered list of authenticators. Each one looks at the ticket and does one of four things:

- **admits** it, returning a user id, optional permissions and an optional expiry;
- **rejects** it (the client gets HTTP 401);
- reports that it is **unavailable**, for example because the identity provider cannot be reached (HTTP 503 with `Retry-After`, so the client retries instead of concluding its credentials are wrong);
- says it is **not its kind** of ticket, and the next authenticator gets a turn.

This lets one tenant accept, say, one-time tickets from browsers and JWTs from services at the same time:

```yaml
tenants:
  - id: acme
    auth:
      - kind: ticket
      - kind: jwt
        algorithm: RS256
        jwks_url: "https://idp.example.com/.well-known/jwks.json"
        issuer: "https://idp.example.com/"
```

Whatever the authenticator, the user id it returns must be 1 to 64 characters of `[A-Za-z0-9_-]`. User ids become part of internal keys, so anything else is rejected and logged.

A tenant without `auth` falls back to the development authenticator and logs a warning at startup. Treat that warning as an error in any environment other than your laptop.

## One-time tickets

This is the simplest option when your users already log in to your backend.

1. The user logs in to your backend the usual way.
2. Your backend calls `POST /api/v1/tickets` on the gateway's server API with the user id and, optionally, room permissions.
3. It returns the ticket (`wpt_…`) to the client.
4. The client connects with it within its lifetime (30 seconds by default, at most 300).

```bash
curl -s -X POST http://127.0.0.1:8081/api/v1/tickets \
  -H "Authorization: Bearer $API_KEY" \
  -d '{"user":"alice","permissions":{"subscribe":["match:*"],"publish":["match:42"]}}'
# {"ticket":"wpt_...","expires_in_s":30}
```

A ticket is consumed the first time it is presented, even when that attempt fails (for instance, against the wrong tenant). A leaked ticket therefore cannot be retried. In a cluster, tickets are stored in Redis (only their SHA-256, never the ticket itself), so a ticket issued by one node is accepted once by any node. If Redis is down, ticket logins fail closed with 503.

The tenant's `auth` must contain `kind: ticket`, otherwise issuing answers 400.

## JWT

If you already issue JSON Web Tokens, the gateway can verify them without calling anyone.

```yaml
      - kind: jwt
        algorithm: HS256
        secret_file: /run/secrets/jwt       # or secret_env; never inline
        issuer: "https://auth.example.com"  # optional; when set, iss is required
        audience: "wsprism"                 # optional; when set, aud is required
        leeway_s: 30
```

- `sub` is the user id. `exp` is required and `nbf` is honoured.
- The algorithm is pinned per authenticator. A token whose header names another algorithm is passed to the next authenticator; it is never verified with a different one. This closes the classic algorithm-confusion attacks.
- HMAC secrets must be at least as long as the hash (32, 48 or 64 bytes).
- With `kid` set, only tokens carrying that key id are considered. Two `jwt` entries with different `kid`s let you rotate a static key without downtime.

wsPrism's own claims sit under a single `wsprism` object so they never collide with an issuer's claims:

```json
{
  "sub": "alice",
  "exp": 1760000000,
  "wsprism": { "tenant": "acme", "subscribe": ["match:*"], "publish": ["match:42"] }
}
```

A token naming another tenant is always rejected. `require_tenant: true` makes the claim mandatory.

### JWKS

With an identity provider such as Auth0, Cognito, Keycloak or Okta, point the gateway at the provider's key set instead of a key file:

```yaml
      - kind: jwt
        algorithm: RS256
        jwks_url: "https://idp.example.com/.well-known/jwks.json"
        jwks_refresh_s: 300
        jwks_min_refetch_s: 30
        jwks_max_stale_s: 86400
```

Keys are fetched on first use and refreshed in the background. A token with an unknown `kid` (the provider just rotated) triggers one immediate refetch, rate limited so that random `kid`s cannot be used to hammer the provider. If refreshing keeps failing, the last good keys stay in use for at most `jwks_max_stale_s`; after that, tokens are answered with 503 rather than trusting keys the provider may have revoked. Symmetric keys published at a URL are never used.

## Connect proxy

When the decision needs your backend's own logic, let the gateway ask:

```yaml
      - kind: proxy
        url: "http://10.0.0.5:9000/wsprism/connect"
        timeout_ms: 2000
        max_in_flight: 256
        secret_env: BACKEND_SECRET
```

For each connection the gateway sends:

```json
POST /wsprism/connect
{"tenant": "acme", "token": "<ticket>", "client_ip": "203.0.113.7"}
```

Your backend admits with

```json
200 {"user": "alice", "permissions": {"subscribe": ["room:*"]}, "expires_at": 1760000000}
```

(`permissions` and `expires_at` are optional) or rejects with 401 or 403. Any other status, a timeout or an unreachable backend becomes 503 with `Retry-After`: an outage of your backend is never reported to the client as bad credentials.

The proxy accepts every token, so it belongs **last** in the chain. The token travels to your backend, so use `https` outside a trusted network.

## Your own authenticator

In Rust, register a factory under a name with `Gateway::builder().auth_plugin(...)`, then refer to it from configuration:

```yaml
    auth:
      - kind: plugin
        name: corp_sso
        options: { realm: "staff" }
```

The factory receives `options` when the gateway is built, so configuration errors surface at startup and in `--check-config`. The authenticator returns an identity, or one of the same three refusals the built-in ones use. See [Rust API](../reference/rust-api.md).

## Permissions

An authenticator can attach two lists to a session:

- `subscribe`: rooms the user may join;
- `publish`: rooms the user may publish to.

Entries are exact names or `prefix*` patterns. An omitted list means unrestricted; an empty list means nothing. A credential without permissions leaves the user bound only by the tenant's policy.

A join needs `subscribe`. Every publish a service makes on a user's behalf, whether from a built-in service like `chat` or the Hot relay or from one of yours, needs `publish`. The check sits in the service context itself, so a custom service cannot forget it.

## Expiry and in-band refresh

When a credential expires (a JWT's `exp`, or `expires_at` from the connect proxy), the gateway closes the session: close code 4001 `auth_expired` on protocol v2. Expiry is checked once a second against a monotonic clock.

To avoid reconnecting, a client can renew its credential on the open connection:

```json
{"v":1,"svc":"sys","type":"refresh","seq":7,"data":{"token":"<new token>"}}
{"v":1,"svc":"sys","type":"refreshed","reply_to":7,"data":{"expires_in_s":900}}
```

The new token goes through the same chain as a new connection and must name the same user. Its permissions replace the old ones at once: rooms the new token no longer allows are left, and the client receives `sys.left` for each. If the refresh fails, the session continues until the old expiry and then closes. Refreshes are limited to one at a time and one per second.

The JavaScript SDK does all of this on its own, at a random point between 70% and 90% of the credential's lifetime so that a fleet of clients does not hit your token service at the same instant.

## Before the credential: admission

Several checks run before any authenticator sees the ticket, cheapest first:

1. **Handshake rate limits**, global and per client address (IPv6 clients are grouped by /64 prefix by default). Excess attempts get 429 with `Retry-After`. Behind a load balancer, list it in `gateway.trusted_proxies` so that the real client address is taken from `X-Forwarded-For`; only the rightmost untrusted hop is used, so clients cannot forge it.
2. **`sid` validation** (400).
3. **Draining**: a node that is shutting down answers 503 so the client goes elsewhere.
4. In a cluster, **overload protection**: when Redis is too slow, new connections are refused with 503 `cluster_busy`.
5. **Tenant lookup** (400) and **protocol negotiation** (400 when the client offers no version the gateway speaks).
6. **Origin check** for browsers against `policy.allowed_origins` (403). Requests without an `Origin` header are not browsers and are not checked.
7. **Tenant capacity** (`max_sessions_total`): 503 with `Retry-After`.

Only then is the ticket read and the chain consulted. Putting authentication last means a refused connection never costs a JWT verification or a call to your backend, and never burns a one-time ticket.
