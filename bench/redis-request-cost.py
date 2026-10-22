#!/usr/bin/env python3
"""Redis CPU per call of a Lua script, sent alone or pipelined.

usage: redis-request-cost.py <script.lua>

Against the test Redis of bench/redis-test.sh only (it FLUSHALLs). Calls
the script (with the session claim's arguments: one key, eight args)
40,000 times: one per round trip on one connection, one per round trip on
16 connections, in pipelines of 8 and of 100, and prints Redis CPU per call
from INFO cpu. Showed that a request arriving alone costs Redis far more
than its script (BENCHMARKS "session churn": 11.6-17.5 vs 2.2 us).
"""
import socket, re, sys, threading
H,P='172.21.0.2',6379
def enc(*a):
    out=[b'*%d\r\n'%len(a)]
    for x in a:
        x=x if isinstance(x,bytes) else str(x).encode(); out.append(b'$%d\r\n%s\r\n'%(len(x),x))
    return b''.join(out)
class C:
    def __init__(s): s.s=socket.create_connection((H,P)); s.f=s.s.makefile('rb')
    def rd(s):
        l=s.f.readline(); t=l[:1]
        if t==b'$':
            n=int(l[1:]); 
            if n>=0: s.f.read(n+2)
        elif t==b'*':
            for _ in range(max(int(l[1:]),0)): s.rd()
        return l
    def info(s,sec):
        s.s.sendall(enc('INFO',sec)); l=s.f.readline(); n=int(l[1:]); return s.f.read(n+2).decode()
ctl=C()
src=open(sys.argv[1]).read()
ctl.s.sendall(enc('SCRIPT','LOAD',src)); l=ctl.f.readline(); n=int(l[1:]); sha=ctl.f.read(n+2)[:-2].decode()
def cpu():
    d=ctl.info('cpu'); return float(re.search(r'used_cpu_user:([\d.]+)',d).group(1))+float(re.search(r'used_cpu_sys:([\d.]+)',d).group(1))
def run(batch, conns, N=40000):
    cs=[C() for _ in range(conns)]
    c0=cpu()
    def work(c, k):
        for i in range(0, N//conns, batch):
            c.s.sendall(b''.join(enc('EVALSHA',sha,1,f'b:us:{{u{k}-{i+j}}}',f'k{i+j}',f's{i+j}','u','node',0,1000000,'deny','b:slot:') for j in range(batch)))
            for _ in range(batch): c.rd()
    ts=[threading.Thread(target=work,args=(c,k)) for k,c in enumerate(cs)]
    [t.start() for t in ts]; [t.join() for t in ts]
    print(f'batch={batch:4d} conns={conns:3d}: redis cpu per call {(cpu()-c0)*1e6/N:.2f} us')
    ctl.s.sendall(enc('FLUSHALL')); ctl.rd()
for b,c in [(1,1),(1,16),(8,1),(100,1)]: run(b,c)
