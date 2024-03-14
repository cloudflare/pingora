# Pingora Internals

（特别感谢 [James Munns](https://github.com/jamesmunns) 撰写此部分）

## 启动 `Server`

Pingora 系统通过生成一个 *服务器* 来启动。服务器负责启动 *服务* 并监听终止事件。

\```
┌───────────┐
┌─────────>│  Service  │
│          └───────────┘
┌────────┐          │          ┌───────────┐
│ Server │──Spawns──┼─────────>│  Service  │
└────────┘          │          └───────────┘
│          ┌───────────┐
└─────────>│  Service  │
└───────────┘
\```

在生成 *服务* 后，服务器继续监听终止事件，并将其传播给创建的服务。

## 服务

*服务* 是负责监听给定套接字并执行核心功能的实体。每个 *服务* 都与特定的协议和一组选项相关联。

> 注意：还有 "后台" 服务，只执行 *工作*，不一定监听套接字。目前我们只讨论监听器服务。

每个服务都有自己的线程池/ tokio 运行时，其中包含的线程数量根据配置的值而定。工作线程不会跨服务共享。服务运行时的线程池可以是工作窃取（tokio 默认）或非工作窃取（N 个独立的单线程运行时）。

\```
┌─────────────────────────┐
│ ┌─────────────────────┐ │
│ │┌─────────┬─────────┐│ │
│ ││  Conn   │  Conn   ││ │
│ │├─────────┼─────────┤│ │
│ ││Endpoint │Endpoint ││ │
│ │├─────────┴─────────┤│ │
│ ││     Listeners     ││ │
│ │├─────────┬─────────┤│ │
│ ││ Worker  │ Worker  ││ │
│ ││ Thread  │ Thread  ││ │
│ │├─────────┴─────────┤│ │
│ ││  Tokio Executor   ││ │
│ │└───────────────────┘│ │
│ └─────────────────────┘ │
│ ┌───────┐               │
└┤Service├───────────────┘
└───────┘
\```

## 服务监听器

在启动时，每个服务都被分配一组要监听的下游端点。单个服务可以监听多个端点。服务器还会传递任何相关的配置，包括 TLS 设置（如果适用）。

这些端点会转换为监听套接字，称为 `TransportStack`。每个 `TransportStack` 都分配给服务执行器中的一个异步任务。

\```
┌───────────────────┐
│┌─────────────────┐│    ┌ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─
┌─────────┐                     ││ TransportStack  ││                                ┌────────────────────┐│
┌┤Listeners├────────┐            ││                 ││    │                       │  ││                    │
│└─────────┘        │            ││ (Listener, TLS  │├──────spawn(run_endpoint())────>│ Service<ServerApp> ││
│┌─────────────────┐│            ││    Acceptor,    ││    │                       │  ││                    │
││    Endpoint     ││            ││   UpgradeFDs)   ││                                └────────────────────┘│
││   addr/ports    ││            │├─────────────────┤│    │                       │  │
││ + TLS Settings  ││            ││ TransportStack  ││                                ┌────────────────────┐│
│├─────────────────┤│            ││                 ││    │                       │  ││                    │
││    Endpoint     ││──build()─> ││ (Listener, TLS  │├──────spawn(run_endpoint())────>│ Service<ServerApp> ││
││   addr/ports    ││            ││    Acceptor,    ││    │                       │  ││                    │
││ + TLS Settings  ││            ││   UpgradeFDs)   ││                                └────────────────────┘│
│└─────────────────┘│            │├─────────────────┤│    │ ┌───────────────┐     │  │ ┌──────────────┐
└───────────────────┘            ││ TransportStack  │├──────spawn(run_endpoint())────>│ Worker Tasks ├ ─ ─ ┘
└───────────────────┘     └───────────────┘          └──────────────┘
\```

## 下游连接生命周期

每个服务通过生成与连接对应的任务来处理传入的连接。只要有新事件需要处理，这些连接就会保持打开状态。

\```
┌ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┐

                                  │  ┌───────────────┐   ┌────────────────┐   ┌─────────────────┐    ┌─────────────┐  │
┌────────────────────┐               │ UninitStream  │   │    Service     │   │       App       │    │  Task Ends  │
│                    │            │  │ ::handshake() │──>│::handle_event()│──>│ ::process_new() │──┬>│             │  │
│ Service<ServerApp> │──spawn()──>   └───────────────┘   └────────────────┘   └─────────────────┘  │ └─────────────┘
│                    │            │                                                    ▲           │                  │
└────────────────────┘                                                                 │         while
│                                                    └─────────reuse                │
┌───────────────────────────┐
└ ─│  Task on Service Runtime  │─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┘
└───────────────────────────┘
\```

## 代理是什么？

有趣的是，`pingora` `Server` 本身并没有特定的代理概念。

相反，它只以 *服务* 为单位思考，这些服务预期包含 `ServiceApp` trait 的特定实现。

例如，这是来自 `pingora-proxy` crate 的 `HttpProxy` 结构如何成为由 `Server` 生成的 `Service` 的一部分：

\```
┌─────────────┐
│  HttpProxy  │
│  (struct)   │
└─────────────┘
│
implements   ┌─────────────┐
│        │HttpServerApp│
└───────>│   (trait)   │
└─────────────┘
│
implements   ┌─────────────┐
│        │  ServerApp  │
└───────>│   (trait)   │
└─────────────┘
│
contained    ┌─────────────────────┐
within     │                     │
└───────>│ Service<ServiceApp> │
│                     │
└─────────────────────┘
\```

在这个表示中，不同的功能和辅助功能在不同的层次提供。

\```
┌─────────────┐        ┌──────────────────────────────────────┐
│  HttpProxy  │        │Handles high level Proxying workflow, │
│  (struct)   │─ ─ ─ ─ │   customizable via ProxyHttp trait   │
└──────┬──────┘        └──────────────────────────────────────┘
│
┌──────▼──────┐        ┌──────────────────────────────────────┐
│HttpServerApp│        │ Handles selection of H1 vs H2 stream │
│   (trait)   │─ ─ ─ ─ │     handling, incl H2 handshake      │
└──────┬──────┘        └──────────────────────────────────────┘
│
┌──────▼──────┐        ┌──────────────────────────────────────┐
│  ServerApp  │        │ Handles dispatching of App instances │
│   (trait)   │─ ─ ─ ─ │   as individual tasks, per Session   │
└──────┬──────┘        └──────────────────────────────────────┘
│
┌──────▼──────┐        ┌──────────────────────────────────────┐
│ Service<A>  │        │ Handles dispatching of App instances │
│  (struct)   │─ ─ ─ ─ │  as individual tasks, per Listener   │
└─────────────┘        └──────────────────────────────────────┘
\```

`HttpProxy` 结构处理代理的高级工作流程

它使用 `ProxyHttp`（注意顺序颠倒！）**trait** 来允许在以下各步骤进行定制（注意：摘自[phase chart](./phase_chart.md)文档）：

```mermaid
 graph TD;
    start("new request")-->request_filter;
    request_filter-->upstream_peer;

    upstream_peer-->Connect{{IO: connect to upstream}};

    Connect--connection success-->connected_to_upstream;
    Connect--connection failure-->fail_to_connect;

    connected_to_upstream-->upstream_request_filter;
    upstream_request_filter --> SendReq{{IO: send request to upstream}};
    SendReq-->RecvResp{{IO: read response from upstream}};
    RecvResp-->upstream_response_filter-->response_filter-->upstream_response_body_filter-->response_body_filter-->logging-->endreq("request done");

    fail_to_connect --can retry-->upstream_peer;
    fail_to_connect --can't retry-->fail_to_proxy--send error response-->logging;

    RecvResp--failure-->IOFailure;
    SendReq--failure-->IOFailure;
    error_while_proxy--can retry-->upstream_peer;
    error_while_proxy--can't retry-->fail_to_proxy;

    request_filter --send response-->logging


    Error>any response filter error]-->error_while_proxy
    IOFailure>IO error]-->error_while_proxy



```

## 放大

在我们放大之前，最好先放大一下，提醒自己代理通常是如何工作的：

```
┌────────────┐          ┌─────────────┐         ┌────────────┐
│ Downstream │          │    Proxy    │         │  Upstream  │
│   Client   │─────────>│             │────────>│   Server   │
└────────────┘          └─────────────┘         └────────────┘
```

代理将从 **下游** 客户端接收连接，并且（如果一切顺利）与适当的 **上游** 服务器建立连接。所选的上游服务器被称为 **peer**。

一旦建立连接，下游和上游可以双向通信。

到目前为止，关于服务器、服务和监听器的讨论集中在这个图表的 **左半部分**，处理传入的下游连接，并将其传递给代理组件。

接下来，我们将看一下这个图表的 **右半部分**，连接到上游的部分。

## 管理上游

与上游peer的连接是通过 `Connector` 进行的。这不是一个特定的类型或 trait，而更像是一种 "风格"。


`Connectors` 负责以下几个方面：

* 与peer建立连接
* 维护与peer的连接池，允许跨以下内容重用连接：

  * 来自单个下游客户端的多个请求
  * 来自不同下游客户端的多个请求
* 测量连接的健康状况，例如定期执行 ping 的连接，对于像 H2 这样的连接
* 处理具有多个可池化层的协议，例如 H2
* 缓存（如果协议和启用了）
* 压缩（如果协议和启用了）

现在在上下文中，我们可以看到代理的每一端是如何处理的：

```
┌────────────┐          ┌─────────────┐         ┌────────────┐
│ Downstream │       ┌ ─│─   Proxy  ┌ ┼ ─       │  Upstream  │
│   Client   │─────────>│ │           │──┼─────>│   Server   │
└────────────┘       │  └───────────┼─┘         └────────────┘
                      ─ ─ ┘          ─ ─ ┘
                        ▲              ▲
                     ┌──┘              └──┐
                     │                    │
                ┌ ─ ─ ─ ─ ┐         ┌ ─ ─ ─ ─ ─
                 Listeners           Connectors│
                └ ─ ─ ─ ─ ┘         └ ─ ─ ─ ─ ─
```

## 多个 peer 怎么办？

`Connectors` 只处理与单个对等体的连接，因此选择多个对等体中的一个实际上是在更高一级的 `ProxyHttp` trait 的 `upstream_peer()` 方法中处理的。

