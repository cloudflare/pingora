# 快速入门：负载均衡器

## 简介

本快速入门向您展示如何使用 pingora 和 pingora-proxy 构建一个简单的负载均衡器。

负载均衡器的目标是对于每个传入的 HTTP 请求，在两个后端之间轮流选择：https://1.1.1.1 和 https://1.0.0.1。

## 构建基本的负载均衡器

为我们的负载均衡器创建一个新的 cargo 项目。我们将其称为 \`load_balancer\`

\```bash
cargo new load_balancer
\```

### 包含 Pingora Crate 和基本依赖项

在项目的 \`cargo.toml\` 文件中添加以下内容到你的依赖项中

\```toml
async-trait="0.1"
pingora = { version = "0.1", features = [ "lb" ] }
\```

### 创建一个 pingora 服务器

首先，让我们创建一个 pingora 服务器。pingora 的 \`Server\` 是一个可以承载一个或多个服务的进程。pingora 的 \`Server\` 负责配置和命令行参数解析、守护进程化、信号处理以及优雅地重启或关闭。

首选的用法是在 \`main()\` 函数中初始化 \`Server\` 并使用 \`run_forever()\` 生成所有运行时线程，并阻塞主线程，直到服务器准备好退出。

\```rust
use async_trait::async_trait;
use pingora::prelude::*;
use std::sync::Arc;

fn main() {
let mut my_server = Server::new(None).unwrap();
my_server.bootstrap();
my_server.run_forever();
}
\```

这将编译并运行，但并不执行任何有趣的操作。

### 创建一个负载均衡器代理

接下来，让我们创建一个负载均衡器。我们的负载均衡器持有一个静态的上游 IP 列表。\`pingora-load-balancing\` crate 已经提供了 \`LoadBalancer\` 结构体，并且提供了常见的选择算法，比如轮询和哈希。所以我们只需使用它。如果使用案例需要更复杂或自定义的服务器选择逻辑，用户可以在这个函数中自行实现。

\```rust
pub struct LB(Arc<LoadBalancer<RoundRobin>>);
\```

为了使服务器成为代理，我们需要为其实现 \`ProxyHttp\` trait。

任何实现 \`ProxyHttp\` trait 的对象本质上都定义了代理中如何处理请求。\`ProxyHttp\` trait 中唯一需要的方法是 \`upstream_peer()\`，它返回应该代理到的地址。

在 \`upstream_peer()\` 的主体中，让我们使用 \`LoadBalancer\` 的 \`select()\` 方法来在上游 IP 之间进行轮询。在这个例子中，我们使用 HTTPS 连接到后端，所以我们还需要在构造 \`Peer\` 对象时指定 \`use_tls\` 和设置 SNI。

\```rust
#[async_trait]
impl ProxyHttp for LB {

    /// 对于这个简单的例子，我们不需要上下文存储
    type CTX = ();
    fn new_ctx(&self) -> () {
        ()
    }

    async fn upstream_peer(&self, _session: &mut Session, _ctx: &mut ()) -> Result<Box<HttpPeer>> {
        let upstream = self.0
            .select(b"", 256) // 哈希对轮询无关紧要
            .unwrap();

        println!("上游节点是：{upstream:?}");

        // 设置 SNI 为 one.one.one.one
        let peer = Box::new(HttpPeer::new(upstream, true, "one.one.one.one".to_string()));
        Ok(peer)
    }
}
\```

为了让 1.1.1.1 后端接受我们的请求，必须存在一个主机头。通过 \`upstream_request_filter()\` 回调可以添加此标头，该回调在与后端的连接建立后、在发送请求标头之前修改请求标头。

\```rust
impl ProxyHttp for LB {
// ...
async fn upstream_request_filter(
&self,
_session: &mut Session,
upstream_request: &mut RequestHeader,
_ctx: &mut Self::CTX,
) -> Result<()> {
upstream_request.insert_header("Host", "one.one.one.one").unwrap();
Ok(())
}
}
\```

### 创建一个 pingora-proxy 服务

接下来，让我们创建一个遵循上述负载均衡器指令的代理服务。

pingora 的 \`Service\` 监听一个或多个 (TCP 或 Unix 域套接字) 端点。当建立新连接时，\`Service\` 将连接交给其“应用程序”。\`pingora-proxy\` 就是这样一个应用程序，它将 HTTP 请求代理到上面配置的后端。

在下面的例子中，我们创建一个带有两个后端 \`1.1.1.1:443\` 和 \`1.0.0.1:443\` 的 \`LB\` 实例。我们通过 \`http_proxy_service()\` 调用将该 \`LB\` 实例放入一个代理 \`Service\`，然后告诉我们的 \`Server\` 托管该代理 \`Service\`。

\```rust
fn main() {
let mut my_server = Server::new(None).unwrap();
my_server.bootstrap();

    let upstreams =
        LoadBalancer::try_from_iter(["1.1.1.1:443", "1.0.0.1:443"]).unwrap();

    let mut lb = http_proxy_service(&my_server.configuration, LB(Arc::new(upstreams)));
        lb.add_tcp("0.0.0.0:6188");

    my_server.add_service(lb);

    my_server.run_forever();
}
\```

### 运行

现在我们已经将负载均衡器添加到服务中，可以使用以下命令运行我们的新项目

\```bash
cargo run
\```

要测试它，只需使用以下命:

```
curl 127.0.0.1:6188 -svo /dev/null
```

要测试它，只需使用以下命令向服务器发送几个请求 [http://localhost:6188](http://localhost:6188)
您还可以将浏览器导航到 http://localhost:6188


以下输出显示负载均衡器正在按照其工作原理在两个后端之间平衡：:
```
upstream peer is: Backend { addr: Inet(1.0.0.1:443), weight: 1 }
upstream peer is: Backend { addr: Inet(1.1.1.1:443), weight: 1 }
upstream peer is: Backend { addr: Inet(1.0.0.1:443), weight: 1 }
upstream peer is: Backend { addr: Inet(1.1.1.1:443), weight: 1 }
upstream peer is: Backend { addr: Inet(1.0.0.1:443), weight: 1 }
...
```

干得好！到目前为止，您已经拥有一个功能齐全的负载均衡器。尽管它是一个 __非常__ 基本的负载均衡器，
但接下来的部分将引导您通过一些内置的 pingora 工具使其更加健壮。



## 添加功能

Pingora 提供了几个有用的功能，可以通过几行代码进行启用和配置。
这些功能范围从简单的peer健康检查到无服务中断地无缝更新运行的二进制文件。



### peer 健康检查


为了使我们的负载均衡器更可靠，我们希望为我们的上游peer添加一些健康检查。这样，如果有一个peer失效，我们就可以迅速停止将流量路由到该peer。



首先让我们看看当一个peer失败时，我们的简单负载均衡器是如何表现的。为此，我们将更新peer列表，包含一个保证损坏的peer。



```rust
fn main() {
    // ...
    let upstreams =
        LoadBalancer::try_from_iter(["1.1.1.1:443", "1.0.0.1:443", "127.0.0.1:343"]).unwrap();
    // ...
}
```

现在，如果我们再次使用 `cargo run` 运行并测试我们的负载均衡器：


```
curl 127.0.0.1:6188 -svo /dev/null
```

我们会发现每3个请求中就有一个失败，返回 `502: Bad Gateway`。这是因为我们的对等体选择严格遵循了我们提供的 `RoundRobin` 选择模式，
而不考虑该对等体是否健康。我们可以通过添加基本的健康检查服务来解决这个问题。



```rust
fn main() {
    let mut my_server = Server::new(None).unwrap();
    my_server.bootstrap();

    // Note that upstreams needs to be declared as `mut` now
    let mut upstreams =
        LoadBalancer::try_from_iter(["1.1.1.1:443", "1.0.0.1:443", "127.0.0.1:343"]).unwrap();

    let hc = TcpHealthCheck::new();
    upstreams.set_health_check(hc);
    upstreams.health_check_frequency = Some(std::time::Duration::from_secs(1));

    let background = background_service("health check", upstreams);
    let upstreams = background.task();

    // `upstreams` no longer need to be wrapped in an arc
    let mut lb = http_proxy_service(&my_server.configuration, LB(upstreams));
    lb.add_tcp("0.0.0.0:6188");

    my_server.add_service(background);

    my_server.add_service(lb);
    my_server.run_forever();
}
```

现在，如果我们再次运行和测试我们的负载均衡器，我们会发现所有请求都成功，并且损坏的对等体从未被使用。
根据我们使用的配置，如果该对等体再次变得健康，它将在 1 秒内重新包含在轮询中。



### 命令行选项

pingora 的 `Server` 类型提供了许多内置功能，我们可以通过单行更改来利用这些功能。



```rust
fn main() {
    let mut my_server = Server::new(Some(Opt::default())).unwrap();
    ...
}
```

通过这个更改，传递给我们的负载均衡器的命令行参数将被 Pingora 使用。我们可以通过运行以下命令来测试：


```
cargo run -- -h
```

我们应该看到一个帮助菜单，列出了现在可用的参数。我们将在下一节中利用这些参数来免费做更多事情。


### 在后台运行


传递参数 `-d` 或 `--daemon` 将告诉程序在后台运行。


```
cargo run -- -d
```

要停止此服务，您可以向其发送 `SIGTERM` 信号进行优雅关闭，在这种情况下，服务将停止接受新请求，但会尝试在退出之前完成所有正在进行的请求。

```
pkill -SIGTERM load_balancer
```
(`SIGTERM` 是 `pkill` 的默认信号。)


### 配置

Pingora 配置文件有助于定义如何运行服务。以下是一个示例配置文件，
定义了服务可以拥有的线程数、pid 文件的位置、错误日志文件以及升级协调套接字的位置（稍后我们将解释此内容）。
复制下面的内容并将其放入您的 `load_balancer` 项目目录中名为 `conf.yaml` 的文件中。



```yaml
---
version: 1
threads: 2
pid_file: /tmp/load_balancer.pid
error_log: /tmp/load_balancer_err.log
upgrade_sock: /tmp/load_balancer.sock
```

要使用此配置文件：

```
RUST_LOG=INFO cargo run -- -c conf.yaml -d
```
`RUST_LOG=INFO` is here so that the service actually populate the error log.

现在您可以找到服务的 pid.

```
 cat /tmp/load_balancer.pid
```

### 优雅地升级服务
（仅适用于 Linux）

假设我们改变了负载均衡器的代码并重新编译了二进制文件。现在我们想要将正在运行的服务升级到这个更新的版本。

如果我们简单地停止旧服务，然后启动新服务，那么在此过程中到达的一些请求可能会丢失。幸运的是，Pingora 提供了一种优雅地升级服务的方法。

这是通过首先向正在运行的服务器发送 `SIGQUIT` 信号，然后使用参数 `-u` 或 `--upgrade` 启动新服务器来完成的。

```
pkill -SIGQUIT load_balancer &&\
RUST_LOG=INFO cargo run -- -c conf.yaml -d -u
```

在此过程中，旧服务器将等待并将其监听套接字移交给新服务器。然后旧服务器继续运行，直到所有正在进行的请求完成。


从客户端的角度来看，服务始终在运行，因为监听套接字从未关闭。


## 完整示例

这个示例的完整代码可以在这个存储库中找到：

[pingora-proxy/examples/load_balancer.rs](../pingora-proxy/examples/load_balancer.rs)

还有其他您可能发现有用的示例在这里：


[pingora-proxy/examples/](../pingora-proxy/examples/)
[pingora/examples](../pingora/examples/)