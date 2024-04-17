# systemd 集成

`Pingora` 服务器不依赖于 `systemd`，但可以很容易地制作成一个 `systemd` 服务。

```ini
[Service]
Type=forking
PIDFile=/run/pingora.pid
ExecStart=/bin/pingora -d -c /etc/pingora.conf
ExecReload=kill -QUIT $MAINPID
ExecReload=/bin/pingora -u -d -c /etc/pingora.conf
```

这个示例 `systemd` 设置将 `Pingora` 的优雅升级集成到 `systemd` 中。要升级 `pingora` 服务，只需安装二进制版本，然后调用 `systemctl reload pingora.service`。
