# Pingora

![Pingora banner image](./docs/assets/pingora_banner.png)

## 什么是Pingora
Pingora是一个Rust框架，用于[构建快速、可靠和可编程的网络系统](https://blog.cloudflare.com/pingora-open-source)。

Pingora经过了数年的实战检验，每秒可处理超过4千万次互联网请求，[详情](https://blog.cloudflare.com/how-we-built-pingora-the-proxy-that-connects-cloudflare-to-the-internet)。

## 特色
* Async Rust: fast and reliable
* HTTP 1/2 end to end proxy
* TLS over OpenSSL or BoringSSL
* gRPC and websocket proxying
* Graceful reload
* Customizable load balancing and failover strategies
* Support for a variety of observability tools

## 使用Pingora的理由
* **安全**是您的首要任务：Pingora是C/C++编写的服务的更安全的替代方案
* 您的服务对**性能要求很高**：Pingora快速高效
* 您的服务需要**大量定制**：Pingora代理框架提供的API具有高度可编程性

# 入门指南

请参阅我们的[快速入门指南](./docs/quick_start.md)，了解构建负载均衡器有多么简单。

我们的[用户指南](./docs/user_guide/index.md)涵盖更多主题，如何配置和运行Pingora服务器，以及如何在Pingora框架之上构建自定义HTTP服务器和代理逻辑。

API文档也适用于所有的crates.

# 本工作区中的重要Crate
* Pingora: the "public facing" crate to build networked systems and proxies
* Pingora-core: this crate defines the protocols, functionalities and basic traits
* Pingora-proxy: the logic and APIs to build HTTP proxies
* Pingora-error: the common error type used across Pingora crates
* Pingora-http: the HTTP header definitions and APIs
* Pingora-openssl & pingora-boringssl: SSL related extensions and APIs
* Pingora-ketama: the [Ketama](https://github.com/RJ/ketama) consistent algorithm
* Pingora-limits: efficient counting algorithms
* Pingora-load-balancing: load balancing algorithm extensions for pingora-proxy
* Pingora-memory-cache: Async in-memory caching with cache lock to prevent cache stampede
* Pingora-timeout: A more efficient async timer system
* TinyUfo: The caching algorithm behind pingora-memory-cache

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

# License
This project is Licensed under [Apache License, Version 2.0](./LICENSE).