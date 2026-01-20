# Pingora Cache Lock Client Disconnect Bug Reproduction

This repository contains a standalone reproduction of a bug in Pingora's cache lock mechanism that causes requests to be held for up to 60 seconds when clients disconnect while waiting on a cache lock.

## Bug Summary

When Pingora's cache lock feature is enabled and multiple concurrent requests hit the same uncached URL:

1. **Writer**: The first request acquires the cache lock and fetches from origin
2. **Readers**: Subsequent requests wait on the cache lock
3. **BUG**: If a reader's downstream client disconnects while waiting:
   - The server-side handler continues waiting on the cache lock
   - Server resources are held until the lock times out (default: 60 seconds)
   - This can cause resource exhaustion under high concurrency

### Impact

- Real user-facing delays when clients disconnect (e.g., page navigation, mobile network issues)
- Server resource exhaustion under high load
- Wasted connection pool capacity
- The issue affects HTTP/1.1, HTTP/2, and indirectly HTTP/3 (where QUIC stream cancellation may not propagate)

## Files

- `src/bin/slow_origin.rs` - A simple origin server that delays responses (configurable)
- `src/bin/proxy.rs` - A minimal Pingora proxy with caching and cache lock enabled
- `src/bin/test_client.rs` - Test client that demonstrates the bug

## Prerequisites

- Rust toolchain (1.70+)
- Pingora source code (this repo should be adjacent to pingora crates)

## Running the Reproduction

### Step 1: Start the slow origin server

```bash
# In terminal 1
RUST_LOG=info cargo run --bin slow-origin -- --port 8000 --delay 5
```

This starts an origin server that delays responses by 5 seconds (configurable via `x-set-sleep` header).

### Step 2: Start the Pingora proxy

```bash
# In terminal 2
RUST_LOG=info cargo run --bin proxy -- --port 6148 --origin-port 8000
```

This starts a Pingora proxy with:
- Caching enabled (in-memory)
- Cache lock enabled with 60-second timeout
- Listening on port 6148

### Step 3: Run the test client

```bash
# In terminal 3
RUST_LOG=info cargo run --bin test-client -- --proxy-port 6148 --num-readers 3
```

## Expected Results

### Without the Fix (BUG present)

```
=== RESULTS ===

Total test time: ~65s

Server close times after client disconnect:
  Min: ~60s
  Max: ~60s
  Avg: ~60s

FAIL: Server took too long to close connections

Max close time: 60.123s
Threshold: 2s

This indicates the bug is present:
  Server waited for cache lock timeout instead of detecting disconnect
```

### With the Fix

```
=== RESULTS ===

Total test time: ~5s (origin delay)

Server close times after client disconnect:
  Min: ~100ms
  Max: ~500ms
  Avg: ~300ms

PASS: Server closed connections quickly after client disconnect

Max close time 423ms < threshold 2s

This indicates the fix is working:
  Server detected client disconnect
  Server exited cache lock wait early
```

## The Fix

The fix involves using `tokio::select!` to race the cache lock wait against client disconnect detection:

```rust
// In proxy_cache.rs
async fn cache_lock_wait_or_disconnect(
    session: &mut Session,
    _ctx: &SV::CTX,
) -> Option<LockStatus> {
    // Skip disconnect detection for subrequests (no downstream client)
    if session.subrequest_ctx.is_some() {
        return Some(session.cache.cache_lock_wait().await);
    }

    // Can only detect disconnect if request body is done
    let body_done = session.as_mut().is_body_done();
    if !body_done {
        return Some(session.cache.cache_lock_wait().await);
    }

    // Race cache lock wait against client disconnect
    tokio::select! {
        biased;

        lock_status = session.cache.cache_lock_wait() => {
            Some(lock_status)
        }

        disconnect_result = session.downstream_session.read_body_or_idle(true) => {
            // Client disconnected
            None
        }
    }
}
```

When a client disconnects:
1. `read_body_or_idle(true)` returns an error
2. `cache_lock_wait_or_disconnect` returns `None`
3. The request handler can clean up and exit early

## Configuration Options

### Slow Origin Server

```
--port <PORT>     Port to listen on [default: 8000]
--delay <DELAY>   Default response delay in seconds [default: 5]
```

### Proxy

```
--port <PORT>               Port to listen on [default: 6148]
--origin-port <PORT>        Origin server port [default: 8000]
--lock-age-timeout <SECS>   Cache lock age timeout [default: 60]
--lock-wait-timeout <SECS>  Cache lock wait timeout [default: 60]
```

### Test Client

```
--proxy-port <PORT>           Proxy port [default: 6148]
--num-readers <N>             Number of reader clients [default: 3]
--disconnect-delay-ms <MS>    Time before disconnecting readers [default: 500]
--threshold-ms <MS>           Success threshold [default: 2000]
--origin-delay <SECS>         Origin delay for writer [default: 5]
```

## Test Variations

### Shorter lock timeout

To demonstrate the bug with a shorter timeout (faster iteration):

```bash
# Start origin with 10s delay
cargo run --bin slow-origin -- --delay 10

# Start proxy (lock times out at 5s)
cargo run --bin proxy -- --lock-age-timeout 5

# Run test
cargo run --bin test-client -- --threshold-ms 1000 --origin-delay 10
```

### Multiple readers

To show resource impact with more concurrent readers:

```bash
cargo run --bin test-client -- --num-readers 20
```

## Related Issues

This bug was discovered while investigating 60-second delays in production systems using Pingora as a caching proxy. The delays were particularly problematic when:

1. Mobile clients with unstable connections disconnected frequently
2. Users navigated away from pages before content loaded
3. HTTP/3 was in use (QUIC stream cancellation didn't propagate to Pingora)

## License

Apache-2.0 (same as Pingora)
