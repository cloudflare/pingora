# Pingora

![Pingora banner image](./docs/assets/pingora_banner.png)

## 什么是Pingora
Pingora是一个Rust框架，用于[构建快速、可靠和可编程的网络系统](https://blog.cloudflare.com/pingora-open-source)。

Pingora经过了数年的实战检验，每秒可处理超过4千万次互联网请求，[详情](https://blog.cloudflare.com/how-we-built-pingora-the-proxy-that-connects-cloudflare-to-the-internet)。

## 特性
* 异步Rust: 快速可靠
* HTTP 1/2 端到端代理
* 基于OpenSSL或BoringSSL的TLS
* gRPC和websocket代理
* 优雅重载
* 可定制的负载均衡和故障转移策略
* 支持多种观测工具(observability tools)

## 使用Pingora的理由
* **安全**是您的首要任务：Pingora是C/C++编写的服务的更安全的替代方案
* 您的服务对**性能要求很高**：Pingora快速高效
* 您的服务需要**大量定制**：Pingora代理框架提供的API具有高度可编程性

# 入门指南

请参阅我们的[快速入门指南](./docs/quick_start.md)，了解构建负载均衡器有多么简单。

我们的[用户指南](./docs/user_guide/index.md)涵盖更多主题，如何配置和运行Pingora服务器，以及如何在Pingora框架之上构建自定义HTTP服务器和代理逻辑。

API文档也适用于所有的crates.

# 本工作区中的重要Crate
* Pingora: 用于构建网络系统和代理的“面向公众”crate
* Pingora-core: 定义协议、功能和基本特性的crate
* Pingora-proxy: 构建HTTP代理的逻辑和API
* Pingora-error: Pingora crate之间使用的通用错误类型
* Pingora-http: HTTP头定义和API
* Pingora-openssl & pingora-boringssl: SSL相关扩展和APIs
* Pingora-ketama: [Ketama](https://github.com/RJ/ketama)一致性算法
* Pingora-limits: 高效计数算法
* Pingora-load-balancing: pingora-proxy的负载均衡算法扩展
* Pingora-memory-cache: 异步内存缓存，配备缓存锁以防止缓存雪崩
* Pingora-timeout: 一个更高效的异步定时器系统
* TinyUfo: pingora-memory-cache背后的缓存算法

# 系统要求

## 系统
Linux 是我们的一级环境和主要重点。

我们将尽力使大多数代码在Unix环境下编译通过。这是为了让开发人员和用户在类似macOS这样的类Unix环境中更轻松地使用Pingora开发（尽管可能会缺少某些功能）。

将支持X86_64和aarch64架构。

## Rust版本

Pingora保持一个滚动的MSRV（最低支持的Rust版本）政策6个月。这意味着只要使用的新Rust版本至少有6个月历史，我们将接受升级MSRV的PR。

我们当前的MSRV是1.72。

# 贡献

请参阅我们的[贡献指南](./.github/CONTRIBUTING.md)。

# 许可
This project is Licensed under [Apache License, Version 2.0](./LICENSE).