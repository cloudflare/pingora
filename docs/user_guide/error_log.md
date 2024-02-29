# Error logging

Pingora libraries are built to expect issues like disconnects, timeouts and invalid inputs from the network. A common way to record these issues are to output them in error log (STDERR or log files).

## Log level guidelines
Pingora adopts the idea behind [log](https://docs.rs/log/latest/log/). There are five log levels:
* `error`: This level should be used when the error stops the request from being handled correctly. For example when the server we try to connect to is offline.
* `warning`: This level should be used when an error occurs but the system recovers from it. For example when the primary DNS timed out but the system is able to query the secondary DNS.
* `info`: Pingora logs when the server is starting up or shutting down.
* `debug`: Internal details. This log level is not compiled in `release` builds.
* `trace`: Fine-grained internal details. This log level is not compiled in `release` builds.

The pingora-proxy crate has a well-defined interface to log errors, so that users don't have to manually log common proxy errors. See its guide for more details.
