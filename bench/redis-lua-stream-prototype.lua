-- M5 prototype (not used by wsPrism): history in a Stream, one PUBLISH per
-- call. Measured with bench/redis-lua-bench.py (CHECK=0); see ADR 0011 "M5".
-- KEYS: state hash, log stream, time marks zset, lengths list
local ch, size, ttl, maxb, pttl = ARGV[1], tonumber(ARGV[2]), tonumber(ARGV[3]), tonumber(ARGV[4]), tonumber(ARGV[6])
local node = 'n:' .. ARGV[7]
local st = redis.call('HMGET', KEYS[1], 'seq', 'ep', 'b', node)
local o, ep, b, done = tonumber(st[1]), st[2], tonumber(st[3] or '0'), tonumber(st[4] or '0')
if not o or not ep then
  o, ep, b, done = 0, ARGV[5], 0, 0
  redis.call('DEL', KEYS[1], KEYS[2], KEYS[3], KEYS[4])
end
local last, base = done, o
local parts, lens = {}, {}
for i = 8, #ARGV, 3 do
  local fseq = tonumber(ARGV[i + 2])
  if fseq > done then
    if fseq > last then last = fseq end
    o = o + 1
    redis.call('XADD', KEYS[2], o .. '-1', 'h', ARGV[i], 'm', ARGV[i + 1])
    parts[#parts + 1] = ARGV[i + 1]
    local n = #ARGV[i + 1]
    lens[#lens + 1] = n
    b = b + n
  end
end
if o > base then
  redis.call('PUBLISH', ch, base .. ':' .. ep .. table.concat(parts))
  redis.call('ZADD', KEYS[3], o, o .. ':' .. redis.call('TIME')[1])
  redis.call('RPUSH', KEYS[4], unpack(lens))
  local n = redis.call('XLEN', KEYS[2])
  if n > size then
    redis.call('XTRIM', KEYS[2], 'MAXLEN', size)
    local gone = redis.call('LPOP', KEYS[4], n - size)
    for i = 1, #gone do b = b - tonumber(gone[i]) end
    redis.call('ZREMRANGEBYSCORE', KEYS[3], '-inf', o - size)
  end
  redis.call('PEXPIRE', KEYS[2], ttl)
  redis.call('PEXPIRE', KEYS[3], ttl)
  redis.call('PEXPIRE', KEYS[4], ttl)
end
redis.call('HSET', KEYS[1], 'seq', o, 'ep', ep, 'b', b, node, last)
redis.call('PEXPIRE', KEYS[1], pttl)
return o
