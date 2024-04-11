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

| 键         | 含义                     | 值类型   |
| ------------- |-------------------| ------|
| version    | 配置文件版本，目前是常量 \`1\` | 数字   |
| pid_file   | pid 文件路径                | 字符串 |
| daemon     | 是否在后台运行服务器          | 布尔值 |
| error_log  | 错误日志输出文件路径。如果未设置，则使用 STDERR | 字符串 |
| upgrade_sock | 升级套接字路径             | 字符串 |
| threads    | 每个服务的线程数             | 数字   |
| user       | 守护进程后 pingora 服务器应以哪个用户身份运行 | 字符串 |
| group      | 守护进程后 pingora 服务器应以哪个用户组身份运行 | 字符串 |
| client_bind_to_ipv4 | 连接服务器时绑定的源 IPv4 地址 | 字符串列表 |
| client_bind_to_ipv6 | 连接服务器时绑定的源 IPv6 地址 | 字符串列表 |
| ca_file    | 根 CA 文件路径               | 字符串 |
| work_stealing | 启用工作窃取运行时（默认为 true）。有关更多信息，请参见 Pingora 运行时（WIP）部分 | 布尔值 |
| upstream_keepalive_pool_size | 连接池中保持的总连接数 | 数字 |


## 扩展

任何未知设置都将被忽略。这允许扩展配置文件以添加和传递用户定义的设置。请参阅用户定义配置部分。
