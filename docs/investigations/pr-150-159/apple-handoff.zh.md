# Apple 部分交接：#154、#155、#157

基线 `4390c78f4467e2047e19690700fbead6a18d8e31`，PR head SHA 见 author-ci.json。
本机没有 Apple 平台运行环境；这三项不能由 Windows/Linux 结果代替。

## #154：CoreAudio 部分读取的静音标志

目的合理：ring 返回部分真实样本并把尾部补零时，不能把整个 buffer 标记为 OUTPUT_IS_SILENCE。
应仅在 read_frames 为 0 时置该标志。该部分未放入审阅分支。

验证步骤：

1. 给 ring 填入少于一次 callback 请求量的非零 PCM，触发真实 CoreAudio callback。
2. 检查真实样本头部保留、尾部为零、OUTPUT_IS_SILENCE 没置位。
3. 空 ring 时检查整段零输出和静音标志。
4. 运行一段音视频，覆盖 pause/resume、倍速和 underrun，核对声音与音频时钟。

## #155：Metal SDR capture 与 packed alpha

目标合理，但不能直接采用现有 patch。BGRA8Unorm capture target 需要与它匹配的 pipeline。
当前 live drawable 在 EDR 模式可能为 RGBA16Float，缓存的 live pipeline 不能直接用于不同 target format。
应使用单独 capture pipeline/cache，并显式选择 SDR 映射与 alpha uniforms。

验证步骤：

1. 在 EDR 开启的实际 Metal layer 播放 HDR 视频，capture 到 SDR PNG；确认 Metal validation 无 format mismatch。
2. 与明确的 SDR tone-mapped 参考图比较中间调和高光；不能只检查 PNG 生成或字节数。
3. 使用 side-by-side/top-bottom packed alpha 视频，检查截图输出颜色、透明区和预乘 alpha。
4. 同一播放器交替 EDR 播放、SDR capture、resize 和再次 capture，检查 live 输出模式与 pipelines 没被截图污染。

## #157：先复现，再设计释放协议

生产调用点均为 `AudioQueueDispose(queue, true)`。官方文档规定 true 同步 disposal，
返回后不再执行 callbacks：

<https://developer.apple.com/documentation/audiotoolbox/audioqueuedispose(_:_:)>。

作者提出“已有 callback 在 dispose 返回后仍访问状态”的前提需要运行证据。
当前计数器方案也不能保护 callback 读取 user_data 后、增加计数前的窗口；等待 100 ms 并可能泄漏不等于完整修复。
未采用该 PR。

验证步骤：

1. 在 iOS/tvOS 实际 AudioQueue callback 入口、所有 state 访问结束、dispose 入／返回、Box drop 记录线程与单调时间。
2. 高频 stop/configure/drop，同步 disposal，使用 ASan 或等效内存诊断；保留出现问题时的最短时间线。
3. 区分真正的 post-dispose 回调与宿主自己在另一路径提前释放 user_data。
4. 如果官方契约不能覆盖实际系统行为，再设计覆盖 callback 入口的稳定 owner／注册表或明确取消协议。

作者 fork CI 精确 head 全部成功，包括 iOS/tvOS 编译；这证明编译覆盖，不证明竞争已复现或修复。
