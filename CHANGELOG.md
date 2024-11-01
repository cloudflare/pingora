# Changelog

All notable changes to this project will be documented in this file.

## [0.4.0](https://github.com/cloudflare/pingora/compare/0.3.0...0.4.0) - 2024-11-01

### 🚀 Features
- [Add preliminary rustls support](https://github.com/cloudflare/pingora/commit/354a6ee1e99b82e23fc0f27a37d8bf41e62b2dc5)
- [Add experimental support for windows](https://github.com/cloudflare/pingora/commit/4aadba12727afe6178f3b9fc2a3cad2223ac7b2e)
- [Add the option to use no TLS implementation](https://github.com/cloudflare/pingora/commit/d8f3ffae77ddc1edd285ab1d517a1b6748ce3d58)
- [Add support for gRPC-web module to bridge gRPC-web client requests to gRPC server requests](https://github.com/cloudflare/pingora/commit/9917177c646a0ab58197f15ec57a3bcbe1e0a201)
- [Add the support for h2c and http1 to coexist](https://github.com/cloudflare/pingora/commit/792d5fd3c14c1cd588b155ddf09c09a4c125a26b)
- [Add the support for custom L4 connector](https://github.com/cloudflare/pingora/commit/7c122e7f36de5c946ac960a1691c5dd41f26e6e6)
- [Support opaque extension field in Backend](https://github.com/cloudflare/pingora/commit/999e379064d2c1266a267abdf9f4f41b14bffcf5)
- [Add the ability to ignore informational responses when proxying downstream](https://github.com/cloudflare/pingora/commit/be97e35031cf4f5a01191f1848bdf491bd9f0d62)
- [Add un-gzip support and allow decompress by algorithm](https://github.com/cloudflare/pingora/commit/e1c6e57db3e613991eda3160d15f81e0669ea066)
- [Add the ability to observe backend health status](https://github.com/cloudflare/pingora/commit/8a0c73f174a27a87c54426a748c4818b10de9425)
- [Add the support for passing sentry release](https://github.com/cloudflare/pingora/commit/07a970e413009ee62fc4c15e0820ae1aa036af22)
- [Add the support for binding to local port ranges](https://github.com/cloudflare/pingora/commit/d1d7a87b761eeb4f71fcaa3f7c4ae8e32f1d93c8)
- [Support retrieving rx timestamp for TcpStream](https://github.com/cloudflare/pingora/commit/d811795938cee5a6eb7cd46399cef17210a0d0c5)

### 🐛 Bug Fixes
- [Handle bare IPv6 address in raw connect Host](https://github.com/cloudflare/pingora/commit/9f50e6ccb09db2940eec6fc170a1e9e9b14a95d0)
- [Set proper response headers when compression is enabled](https://github.com/cloudflare/pingora/commit/55049c4e7983055551b34feee397c736ffc912bb)
- [Check the current advertised h2 max streams](https://github.com/cloudflare/pingora/commit/7419b1967e7686b00aefb7bcd2a4dfe59b31e639)
- Other bug fixes and improvements


### ⚙️ Changes and Miscellaneous Tasks
- [Make sentry an optional feature](https://github.com/cloudflare/pingora/commit/ab1b717bf587723c1c537d6549a8f8096f0900d4)
- [Make timeouts Sync](https://github.com/cloudflare/pingora/commit/18db42cd2cb892432fd7896f0da7e9d19221214b)
- [Retry all h2 connection when encountering graceful shutdown](https://github.com/cloudflare/pingora/commit/11b5882a422774cffbd14d9a9ea7dfc9dc98b02c)
- [Make l4 module pub to expose Connect](https://github.com/cloudflare/pingora/commit/91702bb0c0c5e1f2d5e2f40a19a3f340bb5a6d82)
- [Auto snake case set-cookie header when downgrade to from h2 to http1.1](https://github.com/cloudflare/pingora/commit/2c6190c634f2a5dd2f00e8597902f2b735a9d84f)
- [shutdown h2 connection gracefully with GOAWAYs](https://github.com/cloudflare/pingora/commit/04d7cfeef6205d2cf33ad5704a363ee107250771)
- Other API signature updates

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
