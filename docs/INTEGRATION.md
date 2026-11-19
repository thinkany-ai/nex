# 产品接入 nex

这份文档写给要接入 nex 的产品团队，比如 dscode、termany、agl、fastclaw、weclaw、douchat、workany。读完之后，你的产品应该能做到：**用户在任何网络下，用手机、网页或另一台电脑访问自己机器上的 agent，多台设备同时使用。**

- 协议细节：[`PROTOCOL.md`](./PROTOCOL.md)
- 部署中继：[`DEPLOY.md`](./DEPLOY.md)
- SDK：Rust（`crates/nex`）、TypeScript（`sdk/typescript`）、Go（`sdk/go`）

## 1. 五分钟理解 nex

```
 手机 / 网页 / 另一台电脑（client）          nex 中继                  用户的电脑或 VPS（host）
        │ ① 用设备凭证换一张票据 ──────────▶ │                              │
        │ ② 带票据打开 WebSocket ──────────▶ │ ── ③ 通知：有人要连 agl ───▶ │
        │                                    │ ◀── ④ host 接受，回连中继 ── │ ──▶ 本地服务（agl、终端…）
        │ ◀════════════ ⑤ 双向透传：WebSocket 消息或 TCP 字节 ═══════════▶ │
```

| 概念 | 说明 |
|---|---|
| **账号** | 一个用户。同一个账号下的设备可以互相访问，不同账号之间完全隔离 |
| **设备** | 一个持有凭证（`device_token`）的终端：电脑上的守护进程、手机 App、浏览器都算。`app` 字段记录是哪个产品注册的 |
| **服务** | host 暴露出来的本地服务，有名字和类型：`ws` 透传 WebSocket 消息（浏览器可以直接用），`tcp` 透传原始字节 |
| **host** | 暴露服务的一端，和中继保持一条控制连接，"在线"就是指这条连接还在 |
| **client** | 访问服务的一端。每次打开流之前，先用凭证换一张一次性票据 |

nex **只负责把字节和消息送到对端**，不关心你的业务协议。agl 还是说 agl 协议，终端还是传终端数据。

## 2. 接入前先定三件事

### 2.1 账号从哪里来

| 方式 | 适用场景 | 怎么做 |
|---|---|---|
| **A. 产品后端签发（推荐）** | 产品已经有登录体系，比如 dscode、workany、douchat | 用户登录后，产品后端用应用密钥调用 `POST /v1/admin/devices`，传入**自己的用户 ID**（`external_account_id`），拿到设备凭证后交给这台设备。同一个用户 ID 永远对应同一个 nex 账号 |
| **B. 配对码** | 没有账号体系的工具，比如命令行工具，或者"扫码添加设备"这类场景 | 已登录的设备生成 `XXXX-XXXX` 配对码（也可以显示成二维码），新设备输入或扫码后加入 |

两种方式可以同时用：主流程走 A，"扫码把手机加进来"走 B。

**应用和隔离**：每个产品先在控制台 `/admin` 创建一个应用，拿到应用密钥（`nexk_…`，只显示一次，每个应用最多 2 个有效密钥，用于不停机轮换）。账号按应用隔离，不同应用之间看不到对方的账号和设备。

**跨产品互通**：想让同一个用户在 dscode 和 termany 里的设备互相访问，就让两个产品用**同一个应用**的密钥，并传同一个 `external_account_id`（以后统一身份服务上线后，就是统一的用户 ID）。

### 2.2 设备怎么划分

- **一个凭证同一时间只能有一个 host 连接。** 如果第二个进程用同一个凭证上线，第一个会收到关闭码 **4409**，SDK 会停止它，不会重连。
- 所以：**每个需要上线的进程都注册自己的设备**，注册时填上 `app`（比如 `termany`）。同一台电脑上的 dscode 和 termany 是两台"设备"，设备列表里通过 `app` 区分。
- 只访问别人、自己不上线的 client（比如手机 App、网页），用同一个凭证开多少个流都没问题。

### 2.3 暴露什么服务

| 你的本地服务 | 服务类型 | 写法 |
|---|---|---|
| WebSocket 接口（agl、终端的 PTY 流、JSON-RPC） | `ws` | 指向本地地址，比如 `ws://127.0.0.1:8790/ws` |
| HTTP、SSH、数据库等任意 TCP 服务 | `tcp` | `127.0.0.1:端口` |
| 不想额外开端口，直接在进程里处理 | `ws` 或 `tcp` | Node 和 Go SDK 的 handler 服务：每个流直接交给你的回调 |
| 联通测试 | `ws` | 内置的 `echo` 服务 |

**浏览器能直接用的只有 `ws` 服务**，因为浏览器只能开 WebSocket。如果你的产品要支持网页或手机网页访问，把主接口做成 WebSocket。

## 3. 选 SDK

| 你的代码 | host（暴露服务） | client（访问服务） | 产品后端 |
|---|---|---|---|
| Rust（agl、Tauri 外壳） | `nex::Host` | `nex::NexClient` | `nex::NexAdmin` |
| Node / Electron（termany、dscode、douchat） | `nex-sdk/node` 的 `NexHost` | `nex-sdk` 的 `NexClient` | `nex-sdk/node` 的 `NexAdmin` |
| 浏览器、Next.js 前端（corich、workany 网页版） | 不支持 | `nex-sdk` 的 `NexClient` | 后端用 Node SDK |
| Go（fastclaw、weclaw） | `nex.Host` | `nex.Client` | `nex.Admin` |
| Swift / Kotlin 原生 App | 一般不需要 | 不需要 SDK：一个 HTTP 请求加一个 WebSocket（见 5.4） | 不适用 |
| 其他语言，或者暂时不想改代码 | 旁路进程：`nex up --expose …` | `nex forward` 或 `nex ticket` | 直接调 HTTP |

## 4. host 端：让你的机器上线

host 的职责是保持在线，把别人打开的流接到本地服务上。SDK 已经处理了断线重连（退避 1 到 30 秒）、本地服务连不上时拒绝（client 会收到 4502 和原因），以及吊销和被顶替时自动停止。**你只需要：拿到凭证，声明服务，启动。处理致命错误时提示用户重新登录或配对。**

### Rust

```rust
use nex::{ExposedService, Host, NexClient};
use tokio_util::sync::CancellationToken;

let client = NexClient::new("https://nex.example.com", credential)?; // credential 由你持久化
let services = vec![
    ExposedService::ws("agl", "ws://127.0.0.1:8790/ws")?,
    ExposedService::tcp("ssh", "127.0.0.1:22")?,
    ExposedService::echo(),
];
let cancel = CancellationToken::new();
tokio::spawn(async move {
    if let Err(e) = Host::new(client, services).run(cancel).await {
        // 被吊销（4401）、被另一个进程顶替（4409）、凭证无效（401）
        tracing::error!("nex host stopped: {e:#}");
    }
});
```

### Node / Electron

```ts
import { NexClient, NexHost, wsService, tcpService, handlerService, echoService } from "nex-sdk/node";

const client = new NexClient({ server: "https://nex.example.com", credential });
const host = new NexHost({
  client,
  services: [
    wsService("agl", "ws://127.0.0.1:8790/ws"),
    tcpService("http", "127.0.0.1:5174"),
    // 不开本地端口，直接在进程里处理每个流（socket 是 ws 库的 WebSocket）
    // handler 里要同步挂上 message 监听，避免漏掉第一条消息
    handlerService("term", "ws", (socket, { fromDeviceId }) => attachTerminal(socket, fromDeviceId)),
    echoService(),
  ],
});
const abort = new AbortController();
host.run(abort.signal).catch((err) => {
  // err.code: 'revoked' | 'replaced' | 'bad_request' | 'unauthorized'
  showRelinkPrompt(err);
});
// 默认不打日志，调试时传 logger: console；online/offline 事件可以用来更新界面上的状态
```

### Go

```go
host := &nex.Host{
    Client: client, // nex.NewClient("https://nex.example.com", cred)
    Services: []nex.Service{
        nex.WSService("agent", "ws://127.0.0.1:18789/ws"),
        nex.TCPService("api", "127.0.0.1:8080"),
        nex.EchoService(),
    },
}
if err := host.Run(ctx); err != nil {
    // errors.Is(err, nex.ErrRevoked) / nex.ErrReplaced / nex.ErrUnauthorized
}
```

### 不改代码：旁路进程

```bash
nex join ABCD-EFGH --server https://nex.example.com --name my-mac --app termany
nex up --expose agl=ws://127.0.0.1:8790/ws --echo
```

**macOS 注意**：用 nohup 脱离终端运行的 `nex up`，访问不了**局域网地址**的中继（会报 `No route to host`），这是系统的"本地网络"隐私权限造成的。公网中继不受影响。如果是 App 内嵌的 host，要在 Info.plist 里声明 `NSLocalNetworkUsageDescription`。

## 5. client 端：访问别的设备

流程都一样：先列出设备，选一台在线的，再打开它的某个服务。

### 5.1 Web / Node（TypeScript）

```ts
import { NexClient, CloseCode, describeClose } from "nex-sdk";

const client = new NexClient({ server: "https://nex.example.com", credential });
const devices = await client.devices();             // [{ id, name, app, online, services, is_self }]
const mac = devices.find((d) => d.online && d.app === "dscode");

const ws = await client.open(mac.id, "agl");        // 已经连上的标准 WebSocket
ws.onmessage = (ev) => handleAglMessage(ev.data);   // 接下来直接说 agl 协议
ws.onclose = (ev) => {
  if (ev.code >= 4000) toast(describeClose(ev.code, ev.reason)); // 比如 4502：对方的 agl 没启动
};
ws.send(JSON.stringify({ type: "hello" }));
```

`open()` 在连接建立之前就失败时（比如设备刚好离线），会抛出 `NexStreamError`，带着 `code` 和 `reason`。浏览器自己只能看到 1006，所以失败原因要从这个错误里拿。连接建立之后的失败，从 `onclose` 的关闭码判断。

### 5.2 Go

```go
conn, err := client.Open(ctx, "my-mac", "agent")    // *websocket.Conn（github.com/coder/websocket）
```

### 5.3 Rust 或命令行

```rust
let ws = client.connect("my-mac", "agl").await?;    // tokio-tungstenite 的 WebSocketStream
```

```bash
nex devices
nex ping my-mac                                     # 需要对方开了 echo
nex forward my-mac/ssh --listen 127.0.0.1:2222      # 本地端口转发，然后 ssh -p 2222 localhost
nex ticket my-mac/agl                               # 打印一次性的 ws:// 地址，可以直接用 websocat 连
```

### 5.4 iOS / Android 原生（不需要 SDK）

```swift
// 1. 换票据
var req = URLRequest(url: relay.appending(path: "v1/tickets"))
req.httpMethod = "POST"
req.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
req.setValue("application/json", forHTTPHeaderField: "Content-Type")
req.httpBody = try JSONEncoder().encode(["device": deviceId, "service": "agl"])
let (data, _) = try await URLSession.shared.data(for: req)
let ticket = try JSONDecoder().decode(Ticket.self, from: data)      // { path, ... }
// 2. 打开 WebSocket：https → wss，拼上 ticket.path
var comps = URLComponents(url: URL(string: ticket.path, relativeTo: relay)!, resolvingAgainstBaseURL: true)!
comps.scheme = comps.scheme == "https" ? "wss" : "ws"
let task = URLSession.shared.webSocketTask(with: comps.url!)
task.resume()
```

```kotlin
val body = JSONObject(mapOf("device" to deviceId, "service" to "agl")).toString()
    .toRequestBody("application/json".toMediaType())
val ticket = http.newCall(
    Request.Builder().url("$relay/v1/tickets").header("Authorization", "Bearer $token").post(body).build()
).execute().use { JSONObject(it.body!!.string()) }
val wsUrl = relay.replaceFirst("http", "ws") + ticket.getString("path")
http.newWebSocket(Request.Builder().url(wsUrl).build(), listener)
```

手机 App 切到后台后，系统会断开连接。回到前台时重新换票据、重新打开流就行。**你的业务协议需要能恢复状态**，比如终端靠回放缓冲区，agent 对话靠"从第 N 条消息继续"。

### 5.5 关闭码和错误怎么处理

| 情况 | 表现 | 建议 |
|---|---|---|
| 设备离线 | `/v1/tickets` 返回 404；或者流以 **4404** 关闭 | 显示"设备离线"，在设备列表上标灰 |
| 本地服务没启动 | 流以 **4502** 关闭，reason 里写着原因 | 把原因显示给用户，比如"对方电脑上的 agl 没有运行" |
| host 没响应 | **4504** | 自动重试一次 |
| 流太多 | **4429** | 稍后重试，检查是不是有连接泄漏 |
| 凭证无效或被吊销 | HTTP **401** | 清掉本地凭证，引导用户重新登录或配对 |
| 设备名有重复 | `/v1/tickets` 返回 **409** | 改用设备 id。产品里最好一直用 id，名字只用来显示 |

## 6. 产品后端：管理用户的设备（方式 A）

```ts
// Node 后端。应用密钥只能放在服务端！
import { NexAdmin } from "nex-sdk/node";
const nex = new NexAdmin({ server: "https://nex.example.com", adminKey: process.env.NEX_APP_KEY });

// 用户在某台设备上登录后：
const credential = await nex.issueDevice({
  externalAccountId: user.id,          // 你们自己的用户 ID
  accountName: user.email,
  deviceName: "idoubi 的 MacBook",
  app: "dscode",
});
// → 把 credential 通过登录响应交给这台设备，设备存进钥匙串

// "设备管理"页面：
await nex.devices(user.id);
// 用户点"移除设备"，或者在这台设备上退出登录：
await nex.revoke(user.id, deviceId);
```

Rust 用 `nex::NexAdmin`，Go 用 `nex.NewAdmin`，用法一样。

**设备凭证的生命周期**：

| 时机 | 做什么 |
|---|---|
| 登录 | 签发凭证。每次登录都签发新的，不要在设备之间复用 |
| 退出登录 | 吊销这台设备 |
| 账号被封禁或删除 | 把这个账号下的所有设备逐个吊销 |
| 凭证泄露 | 吊销后重新签发 |

## 7. 配对码流程（方式 B）

1. 已登录的设备调用 `createPairingCode()`，显示 `ABCD-EFGH`，同时生成二维码，内容是 `nex://pair?server=https://nex.example.com&code=ABCD-EFGH`。
2. 新设备扫码或手动输入后，调用 `NexClient.claim(server, { code, deviceName, app })` 拿到凭证并保存。
3. 配对码 10 分钟有效，只能用一次，输入时不区分大小写，`-` 可以省略。

## 8. 各产品的接入建议

| 产品 | host 端 | client 端 | 账号 |
|---|---|---|---|
| **agl / dscode** | 本机守护进程把 agl 暴露成 `ws` 服务 `agl` | 手机 App、网页、其他电脑通过 `open(device, "agl")` 直接说 agl 协议 | 方式 A |
| **termany** | Tauri 的 Rust 部分嵌入 `nex::Host`，暴露 `termany=tcp://127.0.0.1:5174` | 手机 App（Tauri 移动端）用 `forward_tcp` 映射到本机端口，前端代码不用改 | 先用方式 B（扫码），有账号体系后改用 A |
| **fastclaw / weclaw** | Go 守护进程嵌入 `nex.Host`，暴露 agent 接口 | 各产品的 App 或网页 | 方式 A |
| **douchat / workany / corich** | 如果需要连用户本机的 agent，桌面端（Electron 或 Tauri）嵌入 host | 网页和手机端用 `nex-sdk` | 方式 A |

termany 要实现"手机和电脑同时操作同一个终端"，还需要在 termany 服务端支持一个会话同时连接多个客户端。这一项在 termany 里实现，不属于 nex。

## 9. 上线检查清单

- [ ] 中继放在 TLS 后面（`https://`、`wss://`），没有开 `--open-bootstrap`
- [ ] 应用密钥只出现在产品后端，没有打包进任何客户端
- [ ] 设备凭证存在 Keychain、Keystore 或者权限为 0600 的文件里，日志中不会打印
- [ ] 每个上线的进程都有自己的设备，并且填了 `app`
- [ ] host 遇到致命错误（401、4401、4409）时，会提示用户重新登录，而不是静默失败
- [ ] client 对 4404 和 4502 有用户能看懂的提示
- [ ] 业务协议能在流断开后恢复状态（手机切后台、网络切换）
- [ ] 退出登录时会吊销设备
- [ ] 用 `nex ping` 或测试网页验证过：跨网络（手机走 4G）也能连通

## 10. 排错

| 现象 | 原因和处理 |
|---|---|
| host 每隔几秒就上线、下线 | 旧版本 SDK 下两个进程共用了一个凭证。新版会以 4409 停止其中一个，检查日志 |
| `No route to host`（macOS） | 本地网络权限问题，见第 4 节 |
| 网页能列出设备，但 WebSocket 连不上 | 反向代理没开启 WebSocket 升级，见 `DEPLOY.md` |
| 每隔一段时间准时断开 | 代理或负载均衡的空闲超时太短。中继每 20 秒发 ping，代理的超时要大于这个值 |
| 401 | 设备被吊销，或者凭证属于另一个中继（比如国内和海外各一套，凭证不通用） |
| 延迟高 | 现在所有流量都经过中继，延迟取决于两端到中继的距离。用 `nex ping` 测量 |

## 11. 安全模型与限制

- **中继能看到明文**：TLS 只保护"设备到中继"这一段，中继进程本身能看到流的内容。敏感服务要么本身就是加密协议（比如 ssh），要么等端到端加密上线（路线图第 1 项）。**终端类产品对这一点最敏感。**
- **凭证、配对码、票据只以 SHA-256 哈希保存。** 票据 60 秒有效，只能用一次；配对码 10 分钟有效，只能用一次。
- **同一个账号内的设备可以访问彼此暴露的所有服务**，还没有按服务的访问控制。
- **吊销**：新的鉴权立即失效。已有控制连接和流会被断开，跨 Pod 通常在约 1 秒内完成。
- **CORS 全开放**：鉴权用的是请求头里的 Bearer token，没有 cookie，所以不存在 CSRF 问题。
- **限制**：
  - 每台设备最多 128 个并发流，单条消息最大 16 MiB。
  - 还没有按 IP 或账号的限流，上线时要在网关层加。
  - TCP 流不支持半关闭。
  - Kubernetes 支持多副本。经由重启 Pod 的流需要重新打开，host 会自动重连。

## 12. 路线图

1. **端到端加密**：每台设备有自己的密钥对，client 和 host 之间做 Noise 握手，中继只转发密文。
2. **直连优先**：host 上报局域网和 Tailscale 地址，client 先尝试直连，失败再走中继。直连时由 host 自己验证中继签发的票据。
3. **推送**：在中继上登记 APNs 和 FCM token，host 发布"需要处理"这类状态，由中继推送到手机。
4. **按服务的访问控制**：比如手机只能访问 `agl`，不能访问 `ssh`。
5. **多地区部署**：控制面全局一套，数据面按地区部署，host 连最近的中继，票据里带上目标中继的地址。
6. **接入统一身份服务**：`external_account_id` 统一使用身份服务的用户 ID。
