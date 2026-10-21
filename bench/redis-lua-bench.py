#!/usr/bin/env python3
"""Redis-side cost of the history publish script (PUBLISH_KEPT) and variants.

usage: redis-lua-bench.py <script.lua>...   (no argument: the current one
from crates/wsprism-redis/src/lib.rs)

Runs each script against the test Redis (bench/redis-test.sh; never a shared
one: it FLUSHALLs), steady state (every room's log full, so each call
trims), with one pattern subscriber draining the room channels. Prints
Redis CPU per message: EVALSHA usec from INFO commandstats / messages.
Env: REDIS (host:port, default 172.21.0.2:6379), PER_CALL (messages per
call, default 16), CALLS (default 20000), ROOMS (default 240), BODY (message
bytes, default 60).
"""
import os, re, socket, sys, threading, time

HOST, PORT = os.environ.get('REDIS', '172.21.0.2:6379').split(':')
PORT = int(PORT)
PER_CALL = int(os.environ.get('PER_CALL', '16'))
CALLS = int(os.environ.get('CALLS', '20000'))
ROOMS = int(os.environ.get('ROOMS', '240'))
SIZE = 200


def enc(*a):
    out = [b'*%d\r\n' % len(a)]
    for x in a:
        x = x if isinstance(x, bytes) else str(x).encode()
        out.append(b'$%d\r\n%s\r\n' % (len(x), x))
    return b''.join(out)


class Conn:
    def __init__(self):
        self.s = socket.create_connection((HOST, PORT))
        self.buf = b''

    def send(self, data):
        self.s.sendall(data)

    def _line(self):
        while b'\r\n' not in self.buf:
            self.buf += self.s.recv(1 << 20)
        line, self.buf = self.buf.split(b'\r\n', 1)
        return line

    def read(self):
        line = self._line()
        t = line[:1]
        if t == b'-':
            raise RuntimeError(line)
        if t in b'+:':
            return line[1:]
        if t == b'$':
            n = int(line[1:])
            if n < 0:
                return None
            while len(self.buf) < n + 2:
                self.buf += self.s.recv(1 << 20)
            v, self.buf = self.buf[:n], self.buf[n + 2:]
            return v
        if t == b'*':
            return [self.read() for _ in range(int(line[1:]))]
        raise RuntimeError(line)

    def cmd(self, *a):
        self.send(enc(*a))
        return self.read()


def drain(stop):
    c = Conn()
    c.send(enc('PSUBSCRIBE', 'lb:ch:*'))
    c.s.settimeout(0.5)
    while not stop.is_set():
        try:
            c.s.recv(1 << 20)
        except socket.timeout:
            pass


HEAD = b'\x01\x04\x00\x00\x01\x07\x00room-xx\xff\xff'
BODY = b'{"t":"msg","data":"' + b'x' * (int(os.environ.get('BODY', '60')) - 22) + b'"}'


def run(src):
    c = Conn()
    c.cmd('FLUSHALL')
    sha = c.cmd('SCRIPT', 'LOAD', src).decode()
    stop = threading.Event()
    threading.Thread(target=drain, args=(stop,), daemon=True).start()
    time.sleep(0.2)
    seq = 0
    # KEYS: before M5 seq, epoch, log, bytes, applied; since: state, log.
    keys = 'selba' if 'KEYS[5]' in src else 'sl'

    def batch(n):
        nonlocal seq
        out = []
        for i in range(n):
            r = i % ROOMS
            args = ['EVALSHA', sha, len(keys)] + [f'lb:{k}:{{{r}}}' for k in keys] + [
                f'lb:ch:{r}', SIZE, 600000, 1 << 20, f'ep{r}', 86400000, 'node1']
            for _ in range(PER_CALL):
                seq += 1
                args += [HEAD, BODY, seq]
            out.append(enc(*args))
        return b''.join(out)

    def calls(total):
        done = 0
        while done < total:
            n = min(500, total - done)
            c.send(batch(n))
            for _ in range(n):
                c.read()
            done += n

    calls(ROOMS * (SIZE // PER_CALL + 2))  # fill every room's log
    c.cmd('CONFIG', 'RESETSTAT')
    calls(CALLS)
    info = c.cmd('INFO', 'commandstats').decode()
    stop.set()
    m = re.search(r'cmdstat_evalsha:calls=(\d+),usec=(\d+)', info)
    if os.environ.get('CHECK', '1') == '1':  # CHECK=0 for scripts that keep nothing
        assert int(c.cmd('ZCARD', 'lb:l:{0}')) >= SIZE  # == SIZE unless trimming is skipped
    return int(m[2]) / int(m[1]) / PER_CALL


def current():
    lib = os.path.join(os.path.dirname(__file__), '..', 'crates/wsprism-redis/src/lib.rs')
    s = open(lib).read()
    return s.split('const PUBLISH_KEPT: &str = r"', 1)[1].split('";', 1)[0]


scripts = [(p, open(p).read()) for p in sys.argv[1:]] or [('current', current())]
for name, src in scripts:
    r = [run(src) for _ in range(3)]
    print(f'{os.path.basename(name):24s} per_call={PER_CALL} us/msg ' + ' '.join(f'{x:.2f}' for x in r))
