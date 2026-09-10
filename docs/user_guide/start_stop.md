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

### SIGQUIT: graceful upgrade
Similar to SIGTERM, but the server will also transfer all its listening sockets to a new Pingora server so that there is no downtime during the upgrade. See the [graceful upgrade](graceful.md) section for more details.

## Execution Phases 

A Pingora server broadcasts [ExecutionPhase](../../pingora-core/src/server/mod.rs#L60) messages when transitioning between running states. Users can
subscribe to receive updates by calling `Server::watch_execution_phase(&self)`.

Note that Pingora internally manages the creation and shutdown of the tokio Runtime, which is only started as
part of the `Server::run_forever()` method. For transition states where the internal runtime is active, a
runtime handle is passed along with the phase, allowing you to spawn tasks or interact with the async 
runtime. These states are:

- `ExecutionPhase::Running`
- `ExecutionPhase::GracefulUpgradeTransferringFds`
- `ExecutionPhase::GracefulUpgradeCloseTimeout`
- `ExecutionPhase::GracefulTerminate`

A [simple example](../../pingora/examples/execution_phases.rs) of usage.