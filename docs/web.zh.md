# 浏览器播放：MKV、ASS 字体与弹幕

中文 | [English](web.md) | [日本語](web.ja.md)

Web 后端直接属于 liberika。一次构建会同时产出原有 Rust 弹幕引擎、带本仓库补丁的
libass、FreeType、HarfBuzz、FriBidi，以及负责解封装的精简 FFmpeg。应用提供 Canvas
和媒体源，统一通过 `Player` 操作，不需要分别接入弹幕或字幕组件。

这一后端已接入 **Erika Web 应用**，包括播放控件、轨道选择、章节、截图和弹幕设置。
0.3.1 Release 提供独立的 `liberika-web-0.3.1.zip`，包含完整浏览器运行时和
`index.html` 演示页，校验值记录在 `SHA256SUMS`。解压整个归档后通过 HTTPS 或本机
HTTP 服务访问；也可从源码构建。Web 包尚未发布到 npm。

## 在浏览器保留 MKV 字幕和字体

浏览器直接读取原始容器：WASM 内的 FFmpeg 发现音视频轨道、ASS/SSA 字幕和字体附件，
把字幕头、带时间戳的字幕包及字体数据交给 libass。libass 按脚本绘制字体、定位、
描边和特效。服务器提供原始 MKV 和 HTTP Range 即可，不需要先提取字幕、转换成
纯文本或把字幕烧录进画面。本地文件使用同一条播放路径，无需媒体服务器。

字幕由 libass 绘制，弹幕复用原生 Rust DFM 引擎完成解析、文字塑形、排版和碰撞避让。
两者共用播放时钟，并在 liberika 内与视频合成；宿主只需维护一个 `Player` 和 Canvas。
高清字幕、高清弹幕均默认开启，各自使用双倍采样，保持显示大小，不提高视频分辨率。
需要关闭时，分别调用 `player.configureSubtitles({ supersampling: false })` 和
`player.configureDanmaku({ supersampling: false })`；省略该字段会保留当前设置。

| 环节 | 浏览器实现 |
|---|---|
| 读取与解封装 | Worker 分块读取 File/Blob 或 HTTP Range，FFmpeg WASM 提取数据包、轨道、章节和附件 |
| 音视频解码 | 浏览器 WebCodecs 接收压缩数据包 |
| 字幕绘制 | WASM 内的补丁版 libass、FreeType、HarfBuzz、FriBidi，使用 MKV 附带字体 |
| 弹幕绘制 | 编译为 WASM 的原生 Rust DFM 引擎 |
| 同步与呈现 | Web Audio 输出声音并提供时钟，WebGL2 合成视频、字幕和弹幕 |

打开时会检测编码配置、profile 和尺寸是否受支持。
默认向浏览器请求 `prefer-hardware`，再尝试 `no-preference`；
`open(source, { hardwareDecode: false })` 请求 `prefer-software`。
`info.hardwarePreference` 记录请求的偏好，不能作为实际硬解的证明。
不支持的编码会明确返回错误；WASM 包不包含音视频软解或服务端转码回退。

## 构建和运行

需要 Rust、Python 3、Node、CMake、Meson、Ninja、make、pkg-config，以及
Emscripten 4.0.23 和 wasm-bindgen-cli 0.2.126。

```sh
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --version 0.2.126 --locked
git clone https://github.com/emscripten-core/emsdk.git target/web-tools/emsdk
target/web-tools/emsdk/emsdk install 4.0.23
target/web-tools/emsdk/emsdk activate 4.0.23
cargo run -p xtask -- web build
python3 scripts/serve_web.py
```

打开 `http://127.0.0.1:18765`，即可选择本地视频、弹幕和字幕。
完整构建自动获取固定版本依赖、应用补丁、编译和打包；已构建过 C 依赖时，可以使用
`cargo run -p xtask -- web build --skip-native`。Web 中间文件位于
`target/web-build`，完整可部署目录是 `target/web`。不会覆盖原生依赖产物，也不会
修改 shell 启动配置。可以用 `EMSDK` 指定现有 SDK。

发布时保留整个产物目录中的 JS、WASM、Worker 与许可证；回退字体已内嵌在 Rust WASM 中。
浏览器需要 HTTPS 或 localhost；本实现不依赖 SharedArrayBuffer 或跨源隔离。
`build-manifest.json` 记录产物的 SHA-256。示例支持 `?src=URL` 打开远程媒体。

## 接入

将构建产物中的运行时资源复制到网站的 `liberika/` 目录，保持相对路径。
下面的页面可选择本地 MKV，再点击播放；内嵌 ASS 和字体附件自动加载。

```html
<input id="media" type="file">
<button id="play" disabled>播放</button>
<canvas id="video" style="width:100%;height:60vh"></canvas>
<pre id="status"></pre>
<script type="module">
import { Player } from './liberika/index.js';
const fileInput = document.querySelector('#media');
const playButton = document.querySelector('#play');
const status = document.querySelector('#status');
const showError = error => { status.textContent = error.message; };
const player = await Player.create({ canvas: document.querySelector('#video') });
player.addEventListener('error', event => showError(event.detail));
fileInput.onchange = async () => {
  const file = fileInput.files[0];
  if (!file) return;
  fileInput.disabled = true;
  playButton.disabled = true;
  try {
    const info = await player.open(file); // 也可传 Blob 或允许跨域的 URL。
    status.textContent = `${file.name}：${info.duration.toFixed(1)} 秒`;
    playButton.disabled = false;
  } catch (error) {
    showError(error);
  } finally {
    fileInput.disabled = false;
  }
};
// 从用户点击中启动音频，满足浏览器的音频策略。
playButton.onclick = () => player.play().catch(showError);
// 在应用卸载播放器的回调中执行：await player.destroy();
</script>
```

如果播放器通过文件选择器、WASM 或命令队列延迟创建，应在页面初始化时准备音频：

```js
import { AudioSession, Player } from './liberika/index.js';
const audioSession = new AudioSession(); // 用户第一次点击之前
// Canvas 就绪后再创建播放器：
const player = await Player.create({ canvas, audioSession });
```

会话在真实用户输入回调中同步启动音频，预热时保持静音。`play()` 表示请求播放，
`play` 事件表示音频时钟已启动。浏览器尚未允许音频时，seek、pause、close 仍能完成，
不会排在无限等待的 `AudioContext.resume()` 后面。每个播放器独占一个会话；
`destroy()` 同时关闭会话并移除输入监听。

打开媒体后，在同一实例上加载弹幕、外挂字幕或调整样式：

```js
await player.addDanmaku(await danmakuFile.text()); // Bilibili XML / Dandanplay JSON
await player.addSubtitle(await subtitleFile.text()); // ASS / SSA / SRT / WebVTT
await player.configureDanmaku({ opacity: 0.8, fontSize: 30, delay: 0, supersampling: true });
await player.configureSubtitles({ scale: 1, supersampling: true });
```

| 操作 | API 与行为 |
|---|---|
| 播放控制 | `play()`、`pause()`、`seek(seconds)`；跳转保留原有暂停／播放状态 |
| 音量与倍速 | `setVolume(0…1)`、`setRate(0.25…4)` |
| 轨道选择 | `info.tracks`、`selectAudio(id)`、`selectSubtitle(id)`；传 `null` 关闭音轨／字幕 |
| 章节 | `info.chapters` 提供标题 `title` 及以秒为单位的 `start`、`end`（可为 `null`）；用 `seek(chapter.start)` 跳转 |
| 外挂字幕 | `addSubtitle(text)` 替换当前字幕脚本；外部轨道切换由宿主保存脚本并重新加载 |
| 额外字体 | 打开媒体后调用 `addFont(name, bytes)`；ASS 的内嵌字体已自动加载 |
| 弹幕文件 | `addDanmaku(text)` 追加弹幕，返回本次追加数量 |
| 本地即时弹幕 | `sendDanmaku(text)` 在当前播放位置插入弹幕；网络发布由宿主处理 |
| 弹幕偏移 | `configureDanmaku({ delay: seconds })`，范围 −60 到 60 秒，正值延后显示 |
| 截图 | `capture(width?)` 返回含视频、字幕和弹幕的 PNG Blob |
| 关闭与重用 | `close()` 清空当前媒体；`open()` 可在同一实例打开新源 |
| 释放 | `destroy()` 释放 Worker、解码器、帧、GPU 资源和 AudioContext |

完整类型在 `packages/erika_web/index.d.ts`。`loaded`、`stats`、`timeupdate`、`error`、
`log` 是 CustomEvent，数据在 `detail` 中；`play`、`pause`、`ended` 是普通 Event。
每个播放器拥有独立 Worker、WASM 实例和音频上下文。Canvas 只能转交一次，销毁后
创建新播放器需要新 Canvas。打开菜单或切换播放器布局时，应保留原实例和 Canvas；
连续滑块输入可在宿主合并待处理更新。

## 部署

2026-10-22 release 构建实测：播放运行时原始体积 **10.93 MiB**，逐文件 gzip level 9
压缩后 **4.56 MiB**。包含两个 WASM、JavaScript 运行时和完整内嵌回退字体，不含宿主
界面、演示页、文档与媒体。移除重复字体前分别为 17.00 MiB 和 7.30 MiB。
服务器启用 gzip 后可按压缩体积传输；这里统计的是包体积，不是运行时内存。

- 使用 HTTPS 或 localhost，正确提供 JavaScript 模块和 `application/wasm` MIME 类型。
  保留 JS、两个 WASM、Worker 和 `licenses/`；生成的 `index.html` 是可选演示页。
- Canvas 需要非零 CSS 宽高；`Player` 会观察尺寸并更新绘制表面。
- 远程媒体需要允许浏览器请求。超过 8 MiB 的文件需要正确返回 HTTP `206` 和已知总长度，
  跨域时通过 `Access-Control-Expose-Headers` 暴露 `Content-Range`。
  本地测试可用支持 Range 的 `scripts/serve_web.py`。
- 将整套运行时放在版本目录，或让入口与资源一同重新验证缓存，避免新旧 JS/WASM 混用。
- 无需 SharedArrayBuffer 或跨源隔离。音频从用户点击启动，本地媒体通过用户选择的 File/Blob 传入。

## Erika Web 接入实例

[Erika](https://github.com/MCDFsteve/Erika) 的 `web_app` 目标已经使用本后端。
Rust 应用和 Bern 界面运行在浏览器，通过 Bern Web 扩展把媒体命令适配到 liberika
的 `Player`。播放器菜单和迷你播放器共用同一个 Canvas/Worker，本地 File 直接
交给 Worker，字幕和弹幕操作等待异步完成后再更新应用状态。

扩展的构建脚本自动运行 `cargo run -p xtask -- web build`，把完整运行时与应用一起
打包。具体可参考 Erika 的 `web/player.js`、`web/build.py` 和 `docs/web-playback.md`。
其他网站可直接使用上面的 JavaScript API；Bern 只用于 Erika 自身的宿主适配。

## 数据流与性能

本地文件在 Worker 内分块读取；远程大文件通过 2 MiB 的有界缓存按 Range 读取，
不会整部复制进 WASM。跨域服务器需要允许请求并暴露 `Content-Range`。
FFmpeg/libass WASM 模块的内存上限是 512 MiB。
只有压缩视频包从 FFmpeg WASM 传到 WebCodecs，VideoFrame 直接用于 WebGL 纹理上传，
不把视频像素读回 Rust/WASM；底层 GPU 复制方式由浏览器决定。
解码队列有界，跳转、换源、销毁时释放过期帧，并用代际标记隔离旧异步结果。

弹幕复用原生字形图集，不透明度变化不重建图集；GPU 先合并同条弹幕的遮罩，再施加
一次透明度。深色中性文字使用白色描边，彩色文字保留黑色描边。
字号拖动先缩放已有字形，停止 120 ms 后精确重排，连续拖动最多等待 500 ms。
字形图集优先局部上传，ASS 位图未变化时复用纹理；显示 tick 需要确认，避免 Worker
变慢时积压旧动画请求。完整回退字体仅在 `erika_core_bg.wasm` 中内嵌一次，Worker
每次打开媒体时把这份字节注册给 libass，不再打包或请求独立的 `subfont.ttf`。
增量构建也会清除旧版遗留的独立字体文件。离线字体覆盖保持完整，宿主还可以加载自己的字体数据。

## 支持边界

需要浏览器同时提供 WebCodecs 视频／音频解码、OffscreenCanvas、WebGL2 和 Web Audio。
容器包括 MP4/MOV、MKV/WebM、MPEG-TS、AVI、FLV 和 Ogg。
编码适配涵盖 H.264/HEVC 配置记录、VP8/VP9/AV1，以及 AAC/Opus/Vorbis/MP3/FLAC/ALAC，
具体播放能力由浏览器运行时查询决定。
部分封装需要额外码流转换，缺少必要解码配置时会返回错误。

内嵌 ASS 跳转会预读前 5 秒并保留已经见过的事件；首次跳入更早开始的长事件时，
可能暂时缺少该事件。外部完整 ASS 脚本没有这个限制。

目前支持内嵌 ASS/SSA，以及外部 ASS/SSA、SRT、WebVTT；SRT/VTT 作为普通文本转为 ASS，
不保留复杂 VTT 排版。暂不包含位图字幕、内嵌非 ASS 字幕转换、DRM、未知长度直播、
桌面 HDR/EDR/Dolby Vision 和 ArtCNN。当前输出是 SDR，倍速也会改变音调。

## 验证

2026-10-22，Erika Web 完整应用在 macOS 的 Chrome **155.0.8059.85** 中使用合成素材
通过浏览器回归，覆盖：

- H.264/AAC MKV、内嵌 ASS 与字体附件，以及 VP9/Opus WebM。
- 2000 条弹幕，实时字号和透明度、字幕切换、外部 SRT、字幕双倍采样及音轨选择。
- 暂停、跳转、倍速、音量、HTTP Range、容器章节和章节定位。
- 菜单／迷你播放器切换时保持 Canvas/Worker，PNG 截图实际像素检查、关闭后重开、
  窗口尺寸变化，以及坏文件后的恢复。

独立后端回归还使用没有字体附件的 WebM，验证中日文字幕与弹幕可同时显示、没有
独立回退字体请求，并检查共享字节与完整原始字体一致。

Erika 的测试脚本位于 `web/browser.test.mjs`，报告和截图位于该仓库的
`target/web-review/`。同日已在 Safari 27.0.1 使用 1080p HEVC Main 10 / Opus MKV
手动验证打开、内嵌 ASS、暂停、章节定位与连续快进；该文件含两条 ASS 和五份字体附件。
修复了音频启动无限等待堵塞命令队列，以及 HEVC 开放 GOP 在随机访问时把缺少参考帧的
RASL 图像交给解码器而报错的问题。Firefox 尚未验证。

liberika 独立后端的回归步骤：

```sh
node --test packages/erika_web/tests/*.test.mjs
python3 scripts/web_test_fixtures.py
python3 scripts/serve_web.py
# 另一个终端，在本地测试目录安装 Playwright：
npm install --prefix target/web-tests playwright@1.58.2
PLAYWRIGHT_MODULE="$PWD/target/web-tests/node_modules/playwright/index.mjs" \
  node packages/erika_web/tests/browser.mjs
```

测试默认使用本机 Chrome，可设置 `ERIKA_BROWSER_CHANNEL` 指定其他已安装的
Playwright channel，或用 `ERIKA_WEB_URL` 指定测试地址。独立后端的报告与截图写入
`target/web-tests/`，测试会自行生成媒体素材。
