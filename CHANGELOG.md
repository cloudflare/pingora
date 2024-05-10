# Changelog

All notable changes to this project will be documented in this file.

## [0.2.0](https://github.com/cloudflare/pingora/compare/0.1.1...0.2.0) - 2024-05-10

### 🚀 Features
- Add support for downstream h2 trailers and add an upstream h2 response trailer filter
- Add the ability to set TCP recv buf size
- Add a convenience function to retrieve Session digest
- Add `body_bytes_read()` method to Session
- Add `cache_not_modified_filter`
- Add `SSLKEYLOG` support for tls upstream
- Add `Service<HttpProxy<T>>` constructor for providing name
- Add `purge_response` callback
- Make `pop_closed` pub, to simplify DIY drains

### 🐛 Bug Fixes
- Fixed gRPC trailer proxying
- Fixed `response_body_filter` `end_of_stream` always being false
- Fixed compile error in Rust <= 1.73
- Fixed non linux build
- Fixed the counting problem of used_weight data field in `LruUnit<T>`
- Fixed `cargo run --example server` missing cert
- Fixed error log string interpolation outside of proper context
- Fixed tinylfu test flake

### ⚙️ Changes and Miscellaneous Tasks
- API change: `Server::run_forever` now takes ownership and ensures exit semantics
- API change: `cleanup()` method of `ServerApp` trait is now async
- Behavior change: Always return `HttpTask::Body` on body done instead of `HttpTask::done`
- Behavior change: HTTP/1 reason phrase is now parsed and proxied
- Updated `h2` dependency for RUSTSEC-2024-0332
- Updated zstd dependencies
- Code optimization and refactor in a few crates
- More examples and docs

## [0.1.1](https://github.com/cloudflare/pingora/compare/0.1.0...0.1.1) - 2024-04-05

### 🚀 Features
- `Server::new` now accepts `Into<Option<T>>` 
- Implemented client `HttpSession::get_keepalive_values` for Keep-Alive parsing
- Expose `ListenFds` and `Fds` to fix a voldemort types issue
- Expose config options in `ServerConf`, provide new `Server` constructor
- `upstream_response_filter` now runs on upstream 304 responses during cache revalidation
- Added `server_addr` and `client_addr` APIs to `Session`
- Allow body modification in `response_body_filter`
- Allow configuring grace period and graceful shutdown timeout
- Added TinyUFO sharded skip list storage option

### 🐛 Bug Fixes
- Fixed build failures with the `boringssl` feature
- Fixed compile warnings with nightly Rust
- Fixed an issue where Upgrade request bodies might not be handled correctly
- Fix compilation to only include openssl or boringssl rather than both
- Fix OS read errors so they are reported as `ReadError` rather than `ReadTimeout` when reading http/1.1 response headers

### ⚙️ Miscellaneous Tasks
- Performance improvements in `pingora-ketama`
- Added more TinyUFO benchmarks
- Added tests for `pingora-cache` purge
- Limit buffer size for `InvalidHTTPHeader` error logs
- Example code: improvements in pingora client, new LB cluster example
- Typo fixes and clarifications across comments and docs

## [0.1.0] - 2024-02-28
### Highlights
- First Public Release of Pingora 🎉
