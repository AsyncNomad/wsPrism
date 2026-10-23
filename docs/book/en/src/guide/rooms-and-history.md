# Rooms, Presence and History

Rooms are where most realtime applications spend their time: a match, a document, a chat channel, a ticker symbol. This chapter covers joining and leaving, knowing who else is there, and catching up after a disconnect.

## Joining and leaving

```json
{"v":1,"svc":"room","type":"join","room":"match:42","seq":1}
{"v":1,"svc":"sys","type":"joined","room":"match:42","chan":17,"member_id":4097,"reply_to":1}

{"v":1,"svc":"room","type":"leave","room":"match:42","seq":2}
{"v":1,"svc":"sys","type":"left","room":"match:42","reply_to":2}
```

A join succeeds when all of these hold:

- the tenant allows `room:join`;
- the user's `subscribe` permission, if it has one, covers the room;
- the tenant's limits leave room for it: `max_rooms_total` across the tenant, `max_users_per_room` in that room, `max_rooms_per_user` for the user (64 by default, because every joined room costs memory in several indices).

A limit that is reached answers `RESOURCE_EXHAUSTED`. Joining a room twice is harmless and announces nothing.

The `sys.joined` reply carries two identifiers that matter on the Hot lane: `chan`, the room's numeric id on this connection, and `member_id`, which identifies this membership as the sender of Hot frames. Treat both as opaque and expect them to change after a reconnect.

When a session closes for any reason, it leaves all its rooms.

## Publishing

The built-in `chat` service broadcasts to a room the sender has joined:

```json
{"v":1,"svc":"chat","type":"send","room":"match:42","data":{"msg":"gg"}}
```

Your own services publish through their context, and your backend publishes through the server API (`POST /api/v1/publish`). Messages from the server API reach members as `{"svc":"server","type":"publish","room":…,"data":…}`.

Within a room, every member receives Ext messages in the same order, on every node of a cluster.

## Presence

Two ways to know who is in a room:

- **From the backend:** `GET /api/v1/rooms/{room}/presence` lists the users in it (each once, across all nodes in a cluster).
- **From the clients:** with `policy.presence_events: true`, members are told when someone arrives or leaves, and a join reply lists who was already there.

```json
{"v":1,"svc":"sys","type":"member_joined","room":"lobby","data":{"user":"bob","member_id":7}}
{"v":1,"svc":"sys","type":"member_left","room":"lobby","data":{"user":"bob","member_id":7}}
```

Every way out of a room sends `member_left`: an explicit leave, a disconnect, an eviction, a server-side disconnect, and in a cluster the death of the node the member was on (within the node's `slot_ttl_s` plus a few seconds, exactly once). The snapshot in `sys.joined` holds at most 1,000 members and sets `members_truncated` when there are more.

Presence events are off by default, because every join and leave is broadcast to the whole room. In a room of ten thousand, that is a lot of traffic for information most members never look at. Turn them on for rooms where people care who is present: a match, a document, a small channel.

`member_id` matches the `sender` field of that member's Hot frames, so a client can remove a departed player's avatar without any extra bookkeeping.

## History

A room can keep its recent messages so that clients who drop and reconnect receive what they missed. History is configured per tenant with rules matched against room names. The first matching rule applies, and rooms that match none keep nothing.

```yaml
tenants:
  - id: acme
    history:
      - { rooms: "chat:*", size: 200, ttl_s: 600 }
      - { rooms: "orders", size: 1000, ttl_s: 3600, max_bytes: 4194304 }
```

| Field | Default | Range | Meaning |
|---|---|---|---|
| `rooms` | | name or `prefix*` | Rooms the rule covers |
| `size` | 200 | 1 to 10,000 | Messages kept per room |
| `ttl_s` | 600 | 1 to 86,400 | Seconds a message is kept |
| `max_bytes` | 1 MiB | 1 KiB to 64 MiB | Bytes kept per room; the oldest go first |

Budget memory as `max_bytes` times the number of rooms that can match (bounded by `max_rooms_total`). With one node the history lives in the gateway's memory; in a cluster it lives in Redis.

### Offsets and epochs

In a history room every message carries an `offset`, starting at 1 and rising by one per kept message. Offsets belong to an **epoch**, an opaque id for one lifetime of the room's history. `sys.joined` reports the current position:

```json
{"svc":"sys","type":"joined","room":"chat:1","epoch":"9f2c…","offset":44}
```

Presence events, Hot frames and non-JSON messages carry no offset and are not kept.

### Resuming

To resume, join with the last position the client saw:

```json
{"v":1,"svc":"room","type":"join","room":"chat:1","seq":3,"data":{"recover":{"epoch":"9f2c…","offset":41}}}
{"v":1,"svc":"sys","type":"joined","room":"chat:1","reply_to":3,"epoch":"9f2c…","offset":44,"recovered":true}
```

- `"recovered": true`: messages 42 to 44 follow the reply, in order, before anything newer.
- `"recovered": false`: the gap cannot be filled. The epoch changed (the gateway restarted, Redis lost the data, or the room sat empty past its `ttl_s`), or the messages were already dropped by `size`, `max_bytes` or `ttl_s`. Reload the room's state from your backend.

A join without `recover` has no `recovered` field. Clients should drop messages whose offset they have already seen; the JavaScript SDK does this, and resumes automatically on every reconnect.

This is also how a slow consumer gets back on its feet. A client closed with `slow_consumer` reconnects and resumes from its last offset; the replay is not limited by the queue that overflowed.

### Gaps

`sys.gap` tells a client that a room's stream has a hole:

```json
{"v":1,"svc":"sys","type":"gap","room":"lobby"}
{"v":1,"svc":"sys","type":"gap","room":"chat:1","epoch":"a71e…"}
```

Without `epoch`, messages of the room may be missing: in a cluster, the node briefly lost its Redis subscription. Reload the room's state. With `epoch`, the room's history itself restarted (Redis came back empty or from an older snapshot). Offsets begin again in the new epoch, so forget the last offset you saw for the room; otherwise the new, lower offsets look like duplicates. The SDK handles both cases and emits a `gap` event.

## What history is not

History bridges reconnects. It is not storage. Keep the source of truth for anything that matters (chat logs, orders, documents) in your own database, and treat `recovered: false` and `sys.gap` as the signal to reload from it. A gateway that guaranteed durable delivery would need a write-ahead log and replication. That is a database's job, and it would cost every message the latency that wsPrism exists to avoid.
