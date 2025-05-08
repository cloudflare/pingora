# Changelog

All notable changes to this project will be documented in this file.

## [0.5.0](https://github.com/cloudflare/pingora/compare/0.4.0...0.5.0) - 2025-05-09
 
### 🚀 Features

- [Add tweak_new_upstream_tcp_connection hook to invoke logic on new upstream TCP sockets prior to connection](https://github.com/cloudflare/pingora/commit/be4a023d18c2b061f64ad5efd0868f9498199c91)
- [Add ability to configure max retries for upstream proxy failures](https://github.com/cloudflare/pingora/commit/6c5d6021a6e67c971e835bef269655d0db94c2d1)
- [Allow tcp user timeout to be configurable](https://github.com/cloudflare/pingora/commit/e77ca63da58892281f36dcb97c51a8b1e882e2f6)
- [Add peer address to downstream handshake error logs](https://github.com/cloudflare/pingora/commit/3f9e0a2fae8feaea12a1a9687e6b4bf4616f66c5)
- [Allow proxy to set stream level downstream read timeout](https://github.com/cloudflare/pingora/commit/87ae8ce2e7883c0a924a776b193c8a4f858b9349)
- [Improve support for sending custom response headers and bodies for error messages](https://github.com/cloudflare/pingora/commit/a8a6e77eef2c0f4d2a45f00c5b0e316dd373f2f2)
- [Allow configuring multiple listener tasks per endpoint](https://github.com/cloudflare/pingora/commit/69254671148938f6bc467f6decc2fc89ee7f531e)
- [Add get_stale and get_stale_while_update for memory-cache](https://github.com/cloudflare/pingora/commit/bb28044cbe9ac9251940b8a313d970c7d15aaff6)

### 🐛 Bug Fixes
 
- [Fix deadloop if proxy_handle_upstream exits earlier than proxy_handle_downstream](https://github.com/cloudflare/pingora/commit/bb111aaa92b3753e650957df3a68f56b0cffc65d)
- [Check on h2 stream end if error occurred for forwarding HTTP tasks](https://github.com/cloudflare/pingora/commit/e18f41bb6ddb1d6354e824df3b91d77f3255bea2)
- [Check for content-length underflow on end of stream h2 header](https://github.com/cloudflare/pingora/commit/575d1aafd7c679a50a443701a4c55dcfdbc443b2)
- [Correctly send empty h2 data frames prior to capacity polling](https://github.com/cloudflare/pingora/commit/c54190432a2efea30c5a0187bb7d078d33570a43)
- [Signal that the response is done when body write finishes to avoid h1 downstream/h2 upstream errors](https://github.com/cloudflare/pingora/commit/5750e4279e75b1e764dcfc5530aa7a7cebe3abef)
- [Ignore h2 pipe error when finishing an H2 upstream](https://github.com/cloudflare/pingora/commit/8ad15031291eb5779e0e93e714eb969c4132f632)
- [Add finish_request_body() for HTTP healthchecks so that H2 healthchecks succeed](https://github.com/cloudflare/pingora/commit/67bc7cc170e754d335cc1d6d526f203c4345eceb)
- [Fix Windows compile errors by updating `impl<T> UniqueID` to use correct return type](https://github.com/cloudflare/pingora/commit/1756948df77d257bddf7ab798cc3fddf348a91c8)
- [Fixed compilation errors on Windows](https://github.com/cloudflare/pingora/commit/906cb90864bf6e441727083c9cbd4f6fb289d6f5)
- [Poll for H2 capacity before sending H2 body to propagate backpressure](https://github.com/cloudflare/pingora/commit/b6f24ff3725d9d8b6a740d87cad959d94befbe54)
- [Fix for write_error_response for http2 downstreams to set EOS](https://github.com/cloudflare/pingora/commit/c0fa5065812d87e6e404c5624b26cd99c5194079)
- [Always drain v1 request body before session reuse](https://github.com/cloudflare/pingora/commit/fda3317ec822678564d641e7cf1c9b77ee3759ff)
- [Fixes HTTP1 client reads to properly timeout on initial read](https://github.com/cloudflare/pingora/commit/3c7db34acb0d930ae7043290a88bc56c1cd77e45)
- [Fixes issue where if TLS client never sends any bytes, hangs forever](https://github.com/cloudflare/pingora/commit/d1bf0bcac98f943fd716278d674e7d10dce2223e)
 
### Everything Else
 
- [Add builder api for pingora listeners](https://github.com/cloudflare/pingora/commit/3f564af3ae56e898478e13e71d67d095d7f5dbbd)
- [Better handling for h1 requests that contain both transfer-encoding and content-length](https://github.com/cloudflare/pingora/commit/9287b82645be4a52b0b63530ba38aa0c7ddc4b77)
- [Allow setting raw path in request to support non-UTF8 use cases](https://github.com/cloudflare/pingora/commit/e6b823c5d89860bb97713fdf14f197f799aed6af)
- [Allow reusing session on errors prior to proxy upstream](https://github.com/cloudflare/pingora/commit/f8d01278a586c60392b1e3b92e5ed97a415d8fe7)
- [Avoid allocating large buffer in the accept() loop](https://github.com/cloudflare/pingora/commit/ef234f5baa45650be064c7dd34c2f17986361480)
- [Ensure HTTP/1.1 when forcing chunked encoding](https://github.com/cloudflare/pingora/commit/9281cab8eab1b545f15f0e387d2ba4cd2ca27364)
- [Reject if the HTTP header contains duplicated Content-Length values](https://github.com/cloudflare/pingora/commit/eef35768d11305d1293468a6c3ce91a3858dc0fc)
- [proxy_upstream_filter tries to reuse downstream by default](https://github.com/cloudflare/pingora/commit/86293e65b5c7d8a96f3a333a1f191766dc95bee5)
- [Allow building server that avoids std::process::exit during shutdown](https://github.com/cloudflare/pingora/commit/2d977d4eb808d8bcbc0ce87cabac4cf4854dfb80)
- [Update Sentry crate to 0.36](https://github.com/cloudflare/pingora/commit/01a1f9a65c51a4351c29d6961ea3164a6a811958)
- [Update the bounds on `MemoryCache` methods to accept broader key types](https://github.com/cloudflare/pingora/commit/d66923a9a41d00b326cef5dfb57d8c020d6a4abb)
- [Flush already received data if upstream write errors](https://github.com/cloudflare/pingora/commit/aa7c2f1a89a652137a987e5f5dbdab228c2f4d06)
- [Allow modules to receive HttpTask::Done, flush response compression on receiving Done task](https://github.com/cloudflare/pingora/commit/c82fb6ba57b95c256b58095881a33a9bc08f170a)
- API signature changes as part of experimental proxy cache support
- Note MSRV was effectively bumped to 1.82 from 1.72 due to a dependency update, though older compilers may still be able to build by pinning dependencies, e.g. `cargo update -p backtrace --precise 0.3.74`.

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
