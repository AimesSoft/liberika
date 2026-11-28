# Issue #125：资源生命周期修复与验证

验证环境：2026-09-28，macOS / Apple Silicon，分支基于提交 `ca40a95`。
问题链接：https://github.com/AimesSoft/liberika/issues/125

## 结论

问题确实存在，但需要区分两个现象：

- `stop()` 原本有意保留播放会话和解码器，用于从零重播；停止后仍占用一部分内存符合既有语义。
- HTTP 持久预读 worker 和异步 demux worker 缺少确定的所有权收束。HTTP worker 的
  `JoinHandle` 被丢弃，析构只设置标志；卡在响应头或响应体读取时无法观察标志。
  `AsyncDemuxer` 同样没有保存线程句柄，析构只发送 `Stop`。因此对象析构返回时，连接、
  线程和媒体源可能仍然存活。这是已复现的生命周期错误。

没有在原作者的宿主应用中复现“数百 MB 常驻”这一完整曲线，也没有证实 Swift 强引用泄漏。
进程 footprint 不立即回到启动值，本身不能证明仍有活对象。

## 修复

- 前台 Range 请求和持久预读统一通过共享 reqwest/Tokio I/O runtime 执行。每个媒体源有
  带代次的取消令牌；取消会丢弃请求 future，打断 connect、TLS、响应头和响应体等待。
  已进入系统阻塞解析器的 DNS 工作可能独立完成，不应宣称它也已同步退出。
- 前台请求的响应头等待上限为 15 秒，body 连续无数据等待上限为 60 秒。
  每次请求还受整个 fetch 剩余预算约束；重试、退避及 HEAD 回退 GET 合计不超过
  120 秒。回退 GET 使用剩余预算与响应头上限中的较小值。
- 持久流仍保留现有的单连接、条带交接和窗口背压设计。每个流 session 保存 worker
  `JoinHandle`；session 的 `Drop` 统一取消 I/O、唤醒并等待 worker 退出。
  worker 的每个退出路径都通知等待预读的前台线程，避免 socket 已关闭但消费者仍在等待。
- `AsyncDemuxer` 保存 `JoinHandle` 和媒体源取消句柄。seek 中断旧代次的读取，worker
  处理 seek 后才恢复新代次；析构执行终态取消并等待线程退出。等待期间排空 packet
  channel，避免 worker 被满队列卡住。
- `stop()` 仍然可重播，但在返回前释放 HTTP 缓存、持久预读和空播放队列容量。
  现有 `open()` 替换旧会话，`close()` / destroy 释放媒体资源；不新增半关闭 API。

改动限定在 source、demux 生命周期和必要依赖。reqwest/Tokio 增加依赖体积，但让同步
FFmpeg 读取在网络阻塞期间也能取消；仅保存线程句柄并 join 会把网络等待带进析构。
C API、JNI、JSON 桥接、Player 和 Presenter 的公共生命周期接口均保持原样。

没有调用 malloc pressure relief，也没有依靠强制分配器回收来掩盖存活资源。

## 实验

### 修复前的真实 TCP 对照

本地服务器返回一个合法 206 响应头，声明 8 MiB body，但不发送 body。当前 `main` 的
`HttpRangeSource` 启动持久流后立即析构：析构返回后的 500 ms 内，服务器仍未观察到 EOF，
回归测试稳定失败。这证明 worker 和 socket 超过源对象生命周期继续存活。

修复后，同一测试稳定通过；源析构会等待 socket 关闭和 worker 退出。

### 取消、重播和关闭

`crates/erika/tests/media_lifecycle.rs` 使用真实 TCP/HTTP 连接和仓库 MKV 样本验证：

- HEAD、响应头、响应体和 TLS 握手阻塞时均可取消，服务器观察到 EOF/reset；
- 预读在源析构或 `release_buffer()` 时被取消，释放后仍能重新读取并保留认证头；
- 实际 Player 在 HTTP 读取阻塞时执行连续 seek、stop 后重播、open 替换旧会话、close；
- demux 析构等待媒体源释放，packet 队列已满时也不会退出死锁。

本轮 TLS、HEAD、响应头和响应体取消耗时分别为 0.17、0.04、0.06、0.08 ms；
Player stop、open 替换旧会话、close 分别为 0.01、3.38、0.23 ms。
测试上限为 500 ms，这些本机数据不是跨设备延迟承诺。

额外回归 `cancellation_wakes_a_reader_waiting_for_prefetch` 在上一版 `9c8300c` 上
500 ms 内无法收到取消结果，实际失败；补齐退出通知并让条件检查与等待共用锁后通过。

### 超时与重试

6 项真实 TCP 测试使用私有短超时参数验证与生产相同的执行路径：

- HEAD 返回 405 或零长度后，回退 GET 继承 HEAD 重试已消耗的总预算；
- 首次 HEAD 或 GET 卡住时触发单次响应超时，并实际发出第二次请求取得结果；
- body 部分到达后卡住时，释放旧连接，从已收到的偏移续传，并携带 `If-Range`；
- 持续到达的 body 可以超过响应头等待期限；即使持续有数据也不能超出总预算。

前五项在保留原超时逻辑、仅注入短测试预算时全部失败，修复后六项全部通过。
生命周期测试将 EOF、`ConnectionReset` 和 `ConnectionAborted` 都视为有效断连，
覆盖 Windows 可能返回的中止类型；本轮执行环境为 macOS，Windows 尚待复验。

### 内存

macOS 单进程实验使用仓库 MKV、软件解码和 64 KiB HTTP 窗口，执行 12 轮
创建 player → open → play → stop → close。每轮采样前释放测试宿主持有的帧。

| 阶段 | live malloc 字节 | footprint 字节 |
|---|---:|---:|
| 初始基线 | 621,648 | 3,260,800 |
| 第 1 轮播放 | 2,341,024 | 7,356,848 |
| 第 1 轮停止 | 1,675,312 | 7,979,440 |
| 第 1 轮关闭 | 717,552 | 7,881,112 |
| 第 12 轮停止 | 1,675,312 | 8,815,024 |
| 第 12 轮关闭 | 717,552 | 8,700,312 |

12 次关闭后的 live malloc 都是 **717,552 字节**，没有逐轮累积。footprint 没有回到
启动基线，符合分配器、共享 HTTP runtime 和系统框架保留虚拟/物理页的行为；它不能单独
用作泄漏判据。

## 验证命令

```sh
cargo test -p erika -p erika_capi --lib
cargo test -p erika --test media_lifecycle -- --nocapture --test-threads=1
cargo test -p erika --test media_lifecycle memory_probe -- --ignored --nocapture --test-threads=1
```

当前结果：liberika 681 项库测试、C API 41 项测试全部通过；生命周期集成测试 4 项通过、
1 项手动内存实验通过。

## 边界

尚未覆盖原作者宿主、长时间高码率公网 HTTPS、VideoToolbox/Metal 的完整内存曲线和所有
目标平台构建。宿主持有的已交付帧仍需由宿主释放；第三方自定义 `MediaSource` 若存在阻塞
I/O，需要自行实现取消句柄。新增 Rust 依赖的各平台编译仍需要 CI 验证。
