# 用户指南

在本指南中，我们将介绍 `Pingora` 的最常用功能、操作和设置。

## 运行 Pingora 服务器
* [启动和停止](start_stop_zh.md)
* [优雅重启和优雅关闭](graceful_zh.md)
* [配置](conf_zh.md)
* [守护进程化](daemon_zh.md)
* [Systemd 集成](systemd_zh.md)
* [处理 panic](panic_zh.md)
* [异常日志记录](error_log_zh.md)
* [Prometheus](prom_zh.md)

## 构建 HTTP 代理
* [请求的生命周期：`pingora-proxy` 阶段和过滤器](phase_zh.md)
* [`Peer`：如何连接上游](peer_zh.md)
* [使用 `CTX` 跨阶段共享状态](ctx_zh.md)
* [如何返回异常](errors_zh.md)
* [示例：控制请求](modify_filter_zh.md)
* [连接池和复用](pooling_zh.md)
* [处理失败和故障转移](failover_zh.md)

## 高级主题（工作中）
* [Pingora 内部](internals_zh.md)
* 使用 `BoringSSL`
* 用户定义的配置
* `Pingora` 异步运行时和线程模型
* 后台服务
* 异步上下文中的阻塞代码
* Tracing
