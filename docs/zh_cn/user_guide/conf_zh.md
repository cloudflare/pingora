# 配置

```yaml
---
version: 1
threads: 2
pid_file: /run/pingora.pid
upgrade_sock: /tmp/pingora_upgrade.sock
user: nobody
group: webusers
```

## 设置

| Key                          | 含义                     | 值类型   |
|------------------------------|-------------------| ------|
| version                      | 配置文件版本，目前是常量 \`1\` | number   |
| pid_file                     | pid 文件路径                | string |
| daemon                       | 是否在后台运行服务器          | bool |
| error_log                    | 异常日志输出文件路径。如果未设置，则使用 STDERR | string |
| upgrade_sock                 | 升级`Socket`路径             | string |
| threads                      | 每个服务的线程数             | number   |
| user                         | 守护进程后 pingora 服务器应以哪个用户身份运行 | string |
| group                        | 守护进程后 pingora 服务器应以哪个用户组身份运行 | string |
| client_bind_to_ipv4          | 连接服务器时绑定的源 IPv4 地址 | list of string |
| client_bind_to_ipv6          | 连接服务器时绑定的源 IPv6 地址 | list of string |
| ca_file                      | 根 CA 文件路径               | string |
| work_stealing                | 启用工作窃取运行时（默认为 true）。有关更多信息，请参见 Pingora 运行时（WIP）部分 | bool |
| upstream_keepalive_pool_size | 连接池中保持的总连接数 | number |


## 扩展

会自动忽略未定义`Key`。这允许扩展配置文件以添加和传递用户定义的设置。请参阅用户定义配置部分。
