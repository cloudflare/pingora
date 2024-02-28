# Daemonization

When a Pingora server is configured to run as a daemon, after its bootstrapping, it will move itself to the background and optionally change to run under the configured user and group. The `pid_file` option comes handy in this case for the user to track the PID of the daemon in the background.

Daemonization also allows the server to perform privileged actions like loading secrets and then switch to an unprivileged user before accepting any requests from the network.

This process happens in the `run_forever()` call. Because daemonization involves `fork()`, certain things like threads created before this call are likely lost.
