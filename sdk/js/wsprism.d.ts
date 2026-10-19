// Type declarations for wsprism.js (hand-written; keep in sync).

export const PROTOCOL: "wsprism.v2.json";

export class WsPrismError extends Error {
  /** Gateway / backend code (`NOT_ALLOWED`, `UNAVAILABLE`, ...) or a client-side one (`TIMEOUT`, `DISCONNECTED`, `NOT_CONNECTED`, `NOT_JOINED`, `CONNECT_FAILED`, `TICKET_FAILED`). */
  readonly code: string;
  constructor(code: string, message?: string);
}

export type ReconnectPolicy = "stop" | "backoff" | "now";
export function reconnectPolicy(closeCode: number): ReconnectPolicy;
export function seqNewer(a: number, b: number): boolean;

export interface HotFrameOut {
  chan: number;
  svc: number;
  op: number;
  key?: number;
  seq?: number;
  payload?: Uint8Array | ArrayLike<number>;
}
export interface HotFrameIn {
  chan: number;
  svc: number;
  op: number;
  key?: number;
  seq?: number;
  /** Publisher's member id (`0` = the server API). */
  sender?: number;
  payload: Uint8Array;
}
export function encodeHot(f: HotFrameOut): Uint8Array;
export function decodeHot(data: ArrayBuffer | Uint8Array): HotFrameIn | null;

export interface Authed {
  tenant: string;
  user: string;
  sid: string;
}
export interface Member {
  user: string;
  /** Membership id (the Hot `sender` of this member). */
  memberId: number;
}
export interface Joined {
  room: string;
  /** Room id used by Hot frames. */
  chan: number;
  /** This membership's id (the `sender` others see). */
  memberId: number;
  /** Who was already in the room (tenants with `presence_events`; else empty). */
  members: Member[];
  /** More members than the snapshot holds (1000). */
  membersTruncated: boolean;
}
export interface Presence {
  type: "joined" | "left";
  room: string;
  user: string;
  memberId: number;
}
/** Any Ext message: room broadcasts, server publishes, `sys.kicked`, ... */
export interface Message {
  v: number;
  svc: string;
  type: string;
  room?: string;
  data?: unknown;
  reply_to?: number;
  [k: string]: unknown;
}
export interface State extends HotFrameIn {
  /** Room name for `chan`, if this client joined it. */
  room?: string;
}
export interface CloseInfo {
  code: number;
  reason: string;
  wasOpen: boolean;
  willReconnect: boolean;
}

export interface ClientOptions {
  /** `ws(s)://host/v1/ws` */
  url: string;
  tenant: string;
  /** Called before every connection attempt (one-time tickets, fresh JWTs). Must be HTTP token characters (base64url, JWT). */
  getTicket: () => string | Promise<string>;
  sid?: string;
  requestTimeoutMs?: number;
  reconnect?: { enabled?: boolean; minDelayMs?: number; maxDelayMs?: number };
  /** WebSocket implementation (default: `globalThis.WebSocket`). */
  WebSocket?: typeof WebSocket;
}

export interface Events {
  open: Authed;
  close: CloseInfo;
  message: Message;
  state: State;
  /** Someone joined or left a room (tenants with `presence_events`). */
  presence: Presence;
  error: WsPrismError | Error;
  reconnecting: { attempt: number; delayMs: number };
  rejoined: Joined[];
}

export class WsPrismClient {
  constructor(opts: ClientOptions);
  readonly state: "idle" | "connecting" | "open" | "closed";
  readonly user: string | null;
  readonly rooms: Map<string, Joined>;
  /** Returns an unsubscribe function. */
  on<E extends keyof Events>(event: E, fn: (arg: Events[E]) => void): () => void;
  connect(): Promise<Authed>;
  /** Fire-and-forget Ext message; errors arrive as `error` events. */
  send(svc: string, type: string, opts?: { room?: string; data?: unknown }): void;
  /** Ext request that the gateway or a backend answers. */
  request(svc: string, type: string, opts?: { room?: string; data?: unknown; timeoutMs?: number }): Promise<Message>;
  join(room: string): Promise<Joined>;
  leave(room: string): Promise<void>;
  /** Backend RPC (`tenants[].rpc`): the reply's `data`. */
  rpc<T = unknown>(svc: string, type: string, opts?: { room?: string; data?: unknown; timeoutMs?: number }): Promise<T>;
  /** Keyed Hot state to a joined room (`seq` managed per key). */
  sendState(room: string, f: { svc: number; op: number; key?: number; payload: Uint8Array | ArrayLike<number> }): void;
  /** Close for good (no reconnect). */
  close(): void;
}
