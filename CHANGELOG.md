# Changelog

All notable changes to this project will be documented in this file.

## [0.3.0](https://github.com/cloudflare/pingora/compare/0.2.0...0.3.0) - 2024-07-12

### 🚀 Features
- Add support for HTTP modules. This feature allows users to import modules written by 3rd parties.
- Add `request_body_filter`. Now request body can be inspected and modified.
- Add H2c support.
- Add TCP fast open support.
- Add support for server side TCP keep-alive.
- Add support to get TCP_INFO.
- Add support to set DSCP.
- Add `or_err()`/`or_err_with` API to convert `Options` to `pingora::Error`.
- Add `or_fail()` API to convert `impl std::error::Error` to `pingora::Error`.
- Add the API to track socket read and write pending time.
- Compression: allow setting level per algorithm.

### 🐛 Bug Fixes
- Fixed a panic when using multiple H2 streams in the same H2 connection to upstreams.
- Pingora now respects the `Connection` header it sends to upstream.
- Accept-Ranges header is now removed when response is compressed.
- Fix ipv6_only socket flag.
- A new H2 connection is opened now if the existing connection returns GOAWAY with graceful shutdown error.
- Fix a FD mismatch error when 0.0.0.0 is used as the upstream IP

### ⚙️ Changes and Miscellaneous Tasks
- Dependency: replace `structopt` with `clap`
- Rework the API of HTTP modules
- Optimize remove_header() API call
- UDS parsing now requires the path to have `unix:` prefix. The support for the path without prefix is deprecated and will be removed on the next release.
- Other minor API changes

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
