# Connection pooling and reuse

When the request to a `Peer` (upstream server) is finished, the connection to that peer is kept alive and added to a connection pool to be _reused_ by subsequent requests. This happens automatically without any special configuration.

Requests that reuse previously established connections avoid the latency and compute cost of setting up a new connection, improving the Pingora server's overall performance and scalability.

## Same `Peer`
Only the connections to the exact same `Peer` can be reused by a request. For correctness and security reasons, two `Peer`s are the same if and only if all the following attributes are the same
* IP:port
* scheme
* SNI
* client cert
* verify cert
* verify hostname
* alternative_cn
* proxy settings

## Disable pooling
To disable connection pooling and reuse to a certain `Peer`, just set the `idle_timeout` to 0 seconds to all requests using that `Peer`.

## Failure
A connection is considered not reusable if errors happen during the request.
