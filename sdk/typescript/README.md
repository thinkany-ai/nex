# nex-sdk（TypeScript）

nex 中继的 TypeScript SDK。协议见 [`docs/PROTOCOL.md`](../../docs/PROTOCOL.md)，行为与 Rust SDK（`crates/nex`）保持一致。

两个入口：

| 入口 | 运行环境 | 内容 |
|---|---|---|
| `nex-sdk` | 浏览器、类 React Native 的 Web 环境、Node | `NexClient`（REST + 打开流）、类型、关闭码、`describeClose`。只用全局 `fetch` 和 `WebSocket` |
| `nex-sdk/node` | Node ≥ 22（含 Electron 主进程） | 上面全部，加上 `NexHost`（让设备上线并暴露服务）、`NexAdmin`（产品后端）、`forwardTcp`、`openStream` |

## 安装

包名暂时是占位的 `nex-sdk`，还没发布（`private: true`）。在仓库内先构建：

```bash
cd sdk/typescript
npm install
npm run build      # 输出到 dist/，带 .d.ts
npm test           # 构建后跑测试，需要 target/release/nex-server 和 nex
```

产品里可以用 `"nex-sdk": "file:../nex/sdk/typescript"` 引用。`ws` 是唯一的运行时依赖，只有 `nex-sdk/node` 用到。

## 用法一：浏览器 / 移动 Web 客户端（只做 client）

设备凭证从配对码或产品后端拿到，保存好（`device_token` 相当于密码）。

```ts
import { NexClient, CloseCode, describeClose } from "nex-sdk";

// 用配对码加入账号（只需一次），之后把 credential 存起来
const credential = await NexClient.claim("https://nex.example.com", {
  code: "ABCD-EFGH", // 大小写和 "-" 都无所谓
  deviceName: "我的手机",
  app: "douchat",
});

const client = new NexClient({ server: "https://nex.example.com", credential });

const devices = await client.devices(); // [{ id, name, online, services, ... }]

// 打开到某台设备上某个服务的流；WebSocket 打开后才 resolve，binaryType 是 arraybuffer
const ws = await client.open("mbp", "agl"); // 设备可以写 id 或名字
ws.addEventListener("message", (ev) => console.log(ev.data));
ws.addEventListener("close", (ev) => {
  if (ev.code >= 4000) alert(describeClose(ev.code, ev.reason)); // 4404/4429/4502/4504
});
ws.send("hello");
```

- `ws` 类型的服务：文本帧和二进制帧原样转发到对端的本地 WebSocket 服务。
- `tcp` 类型的服务：用二进制帧收发原始字节，任何一方关闭就是整个连接结束。
- 中继地址可以带路径前缀，比如 `https://example.com/nex`。
- 接口失败抛出 `NexApiError`（带 `status`）。`401` 表示凭证失效或被吊销，应该让用户重新配对或登录。
- 握手阶段就失败（票据过期、网络不通）时，`open` 抛出 `NexStreamError`。
- 其他接口：`whoami()`、`createPairingCode()`、`rename(id, name)`、`revoke(id)`、`ticket(device, service)`、`wsUrl(path)`。

## 用法二：在产品的 Node 服务里当 host（termany、dscode、douchat 主进程等）

host 让本机上线，并把本地服务暴露给同账号的其他设备。**一个凭证同一时间只能有一个 host 连接**，多个进程要各自注册设备。

```ts
import { NexClient, NexHost, NexFatalError, tcpService, wsService, echoService, handlerService } from "nex-sdk/node";

const client = new NexClient({ server: "https://nex.example.com", credential });

const host = new NexHost({
  client,
  services: [
    tcpService("ssh", "127.0.0.1:22"),
    wsService("agl", "ws://127.0.0.1:8790/ws"),
    echoService(), // 名字是 echo，`nex ping` 用它测延迟
    // 进程内直接处理，不用开本地端口。socket 是 ws 包的 WebSocket，已经打开。
    // message 监听要同步挂上（在此之前到达的消息会在 handler 返回后补发）。
    handlerService("term", "ws", (socket, { fromDeviceId, streamId }) => {
      socket.on("message", (data, isBinary) => socket.send(data, { binary: isBinary }));
    }),
  ],
  logger: console, // 可选，默认不输出
  onOnline: (deviceId) => console.log("online", deviceId),
  onOffline: (err) => console.log("offline", err?.message), // 主动停止时 err 为 null
});

const ac = new AbortController();
try {
  await host.run(ac.signal); // abort 后 resolve
} catch (err) {
  if (err instanceof NexFatalError) {
    // err.code: "revoked" | "unauthorized"（清掉凭证，重新配对）
    //           "replaced"（同一个凭证在别处上线了，不要重启）| "bad_request"（代码问题）
  }
}

// 运行中可以随时更新服务列表，会立刻重新发送 hello
host.updateServices([...host.services, tcpService("db", "127.0.0.1:5432")]);
```

- host 也实现了 `EventEmitter`：`host.on("online", ...)`、`host.on("offline", ...)`。
- 收到连接请求时先连本地服务，连不上就拒绝，对方收到关闭码 4502，reason 里写着原因。
- 断线自动重连：退避从 1 秒开始翻倍，最长 30 秒；上一次连接稳定超过 30 秒就重置为 1 秒。遇到 HTTP 401、4401、4400、4409 就停止，`run()` 以 `NexFatalError` 拒绝。
- `run()` 结束时会关掉控制连接和它服务过的所有流。

在 Node 里访问别的设备，除了 `client.open()`（全局 WebSocket），还可以用：

```ts
import { openStream, forwardTcp } from "nex-sdk/node";

const ws = await openStream(client, "vps", "agl"); // ws 包的 WebSocket

// 本地端口转发到对方的 tcp 服务，每个本地连接一条流
const server = await forwardTcp(client, "vps", "ssh", { port: 2222 }); // host 默认 127.0.0.1
// ssh -p 2222 127.0.0.1 ；不用了就 server.close()
```

## 用法三：产品后端用 NexAdmin 给用户发设备凭证（corich、workany 等）

admin key 只能放在服务端。同一个 `externalAccountId` 拿到的设备都在同一个 nex 账号下，不同产品之间也能互相访问。

```ts
import { NexAdmin } from "nex-sdk/node";

const admin = new NexAdmin({ server: "https://nex.example.com", adminKey: process.env.NEX_ADMIN_KEY! });

// 用户登录后给他的设备发凭证，返回给客户端保存
const credential = await admin.issueDevice({
  externalAccountId: user.id, // 路径里会自动做 URL 编码
  accountName: user.name,
  deviceName: "MacBook Pro",
  app: "corich",
});

const devices = await admin.devices(user.id);
await admin.revoke(user.id, devices[0].id); // 立即断开
```

中继配置了 admin key 时，`NexClient.bootstrap(server, { accountName, deviceName, adminKey })` 也需要这个 key，否则返回 403。

## 测试

`npm test` 会构建，然后用 `node:test` 跑 `test/` 下的测试。每个测试文件在一个空闲端口上启动真实的 `target/release/nex-server --db :memory:`，结束后关掉。如果二进制不存在，先在仓库根目录运行 `cargo build --release`。也可以用环境变量 `NEX_SERVER_BIN`、`NEX_CLI_BIN` 指定路径。

覆盖：配对和 bootstrap、tcp/ws/echo/handler 服务的并发访问、`forwardTcp`、4502/4404、跨账号隔离、吊销、凭证被顶替、admin API、断线重连，以及与 Rust CLI 的互通（`nex up --echo`、`nex ping`、`nex forward`、`nex join`）。
