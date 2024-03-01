# Graceful restart and shutdown

Graceful restart, upgrade, and shutdown mechanisms are very commonly used to avoid errors or downtime when releasing new versions of Pingora servers.

Pingora graceful upgrade mechanism guarantees the following:
* A request is guaranteed to be handled either by the old server instance or the new one. No request will see connection refused when trying to connect to the server endpoints.
* A request that can finish within the grace period is guaranteed not to be terminated.

## How to graceful upgrade
### Step 0
Configure the upgrade socket. The old and new server need to agree on the same path to this socket. See configuration manual for details.

### Step 1
Start the new instance with the `--upgrade` cli option. The new instance will not try to listen to the service endpoint right away. It will try to acquire the listening socket from the old instance instead.

### Step 2
Send SIGQUIT signal to the old instance. The old instance will start to transfer the listening socket to the new instance.

Once step 2 is successful, the new instance will start to handle new incoming connections right away. Meanwhile, the old instance will enter its graceful shutdown mode. It waits a short period of time (to give the new instance time to initialize and prepare to handle traffic), after which it will not accept any new connections.
