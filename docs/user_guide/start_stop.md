# Starting and stopping Pingora server

A pingora server is a regular unprivileged multithreaded process.

## Start
By default, the server will run in the foreground.

A Pingora server by default takes the following command-line arguments:

| Argument      | Effect        | default|
| ------------- |-------------| ----|
| -d, --daemon | Daemonize the server | false |
| -t, --test | Test the server conf and then exit (WIP) | false |
| -c, --conf | The path to the configuration file | empty string |
| -u, --upgrade | This server should gracefully upgrade a running server | false |

## Stop
A Pingora server will listen to the following signals.

### SIGINT: fast shutdown
Upon receiving SIGINT (ctrl + c), the server will exit immediately with no delay. All unfinished requests will be interrupted. This behavior is usually less preferred because it could break requests.

### SIGTERM: graceful shutdown
Upon receiving SIGTERM, the server will notify all its services to shutdown, wait for some preconfigured time and then exit. This behavior gives requests a grace period to finish.

Listening services stop accepting new connections when shutdown starts. A service that must stay reachable during the grace period, such as a readiness or metrics endpoint, can opt in to keep accepting with `Service::keep_listening_on_terminate()`, behind the `keep_listening_on_terminate` cargo feature ([example](../../pingora-proxy/examples/keep_listening_on_terminate.rs)). The application still reports draining itself, for example from a background service's `ShutdownWatch`.

| Signal | Default listener | `keep_listening_on_terminate` |
| ------ | ---------------- | ----------------------------- |
| SIGTERM | Stops accepting | Accepts until the grace period ends |
| SIGQUIT | Transfers its socket, then stops accepting | Same as default |

### SIGQUIT: graceful upgrade
Similar to SIGTERM, but the server will also transfer all its listening sockets to a new Pingora server so that there is no downtime during the upgrade. See the [graceful upgrade](graceful.md) section for more details.
