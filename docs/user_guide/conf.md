# Configuration

A Pingora configuration file is a list of Pingora settings in yaml format.

Example
```yaml
---
version: 1
threads: 2
pid_file: /run/pingora.pid
upgrade_sock: /tmp/pingora_upgrade.sock
user: nobody
group: webusers
```
## Settings
| Key      | meaning        | value type |
| ------------- |-------------| ----|
| version | the version of the conf, currently it is a constant `1` | number |
| pid_file | The path to the pid file | string |
| daemon | whether to run the server in the background | bool |
| error_log | the path to error log output file. STDERR is used if not set | string |
| upgrade_sock | the path to the upgrade socket. | string |
| threads | number of threads per service | number |
| user | the user the pingora server should be run under after daemonization | string |
| group | the group the pingora server should be run under after daemonization | string |
| client_bind_to_ipv4 | source IPv4 addresses to bind to when connecting to server | list of string |
| client_bind_to_ipv6 | source IPv6 addresses to bind to when connecting to server| list of string |
| ca_file | The path to the root CA file | string |
| work_stealing | Enable work stealing runtime (default true). See Pingora runtime (WIP) section for more info | bool |
| upstream_keepalive_pool_size | The number of total connections to keep in the connection pool | number |

## Extension
Any unknown settings will be ignored. This allows extending the conf file to add and pass user defined settings. See User defined configuration section.
