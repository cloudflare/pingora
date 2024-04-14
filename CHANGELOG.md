# Changelog

All notable changes to this project will be documented in this file.

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
