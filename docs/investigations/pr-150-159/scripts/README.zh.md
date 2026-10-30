# 复现原生检查

[English](README.md) · [日本語](README.ja.md)

这些独立宿主使用实际二进制文件，补充基线/head 回归日志。OHOS 模拟器的真实运行结果记录在 [鸿蒙模拟器运行记录](../ohos-runtime.zh.md)；Apple 验证另行进行。

## Windows

按照 `docs/building.zh.md` 的原生依赖说明，使用 MSVC、LLVM 和 Windows SDK 构建本分支的 `erika_capi`，并安装 Python 3。将生成的 DLL 放在 `scripts/target/debug/erika_capi.dll`，将 [原审阅报告](../review.zh.md) 中描述的 BBB 素材下载到 `scripts/bbb-10s.mp4`。然后在 PowerShell 中运行：

```powershell
python .\windows-live-capi.py C:\path\to\Erika
```

宿主创建 Win32 HWND，附着生产 D3D11 渲染器，播放本地媒体以及使用小缓存窗口的 HTTP 206/整文件 200 响应，并测试暂停、EOF seek、恢复、速率、MP3 探测和畸形输入。默认运行需要真实的默认 WASAPI endpoint；没有 endpoint 时传 `--video-only` 明确省略音频播放。D3D11 未实现可选的 C API 截图，此限制会记录在日志中。

## Linux

记录的环境为现有 `erika-performance-ci:20260930` 镜像中的 Ubuntu 26.04、Rust 1.93.1、FFmpeg 8、打补丁的 libass、Xvfb、PulseAudio 和 Mesa llvmpipe。使用 libass 和 wgpu 构建本分支：

```sh
export CARGO_TARGET_DIR=/target
cargo build --locked -p erika_capi -p linux_native_demo
cc examples/linux_native_demo/capi_smoke.c -I crates/erika_capi/include \
  -L /target/debug -Wl,-rpath,/target/debug -lerika_capi -lX11 \
  -o /target/debug/linux_capi_smoke
```

脚本假设 checkout 在 `/work`、二进制文件在 `/target/debug`、脚本/媒体/输出在 `/review-logs`。使用非 root 账户、权限 0700 的可写 `/tmp/erika-pr-desktop` 目录以及 PulseAudio null sink。

```sh
xvfb-run -a bash /review-logs/native-desktop.sh
xvfb-run -a bash /review-logs/linux-live-subtitle.sh whitespace-separators.srt
```

创建含两条 SRT 字幕的 sidecar：`First` 为 1–2 秒，`Second` 为 3–4 秒，分隔行含 `空格 + tab + 空格`。宿主短暂恢复 seek 后捕获暂停帧；检查 `whitespace-separators-second.png` 中可见的 `Second`。像素计数也包含切换轨道可能引起的视频刷新，并非字幕面积。

Python 宿主将带封面的 1 秒 MP3 播放 4 秒并断言实际音频消费；共消费 48000 帧，最终队列为空且音频错误为零。最初较短的 1.5 秒观察未测到消费，不足以判断输出。独立原生 demo 在 5 秒 MP3 上消费了音频，但其通用 smoke 退出检查要求视频，对纯音频文件以 1 退出；`linux-cover-audio.sh` 保留该结果。

用仓库的带封面 MP3 生成两个样本：

```sh
ffmpeg -y -stream_loop -1 -i /work/crates/erika/testdata/software/mp3-attached-pic.mp3 \
  -t 5 -map 0:a -map 0:v -c:a libmp3lame -c:v copy \
  -disposition:v attached_pic /review-logs/cover-5s.mp3
ffmpeg -y -i /review-logs/cover-5s.mp3 -map 0:a -c copy /review-logs/no-cover-5s.mp3
xvfb-run -a bash /review-logs/linux-cover-audio.sh
```

## OHOS SDK 构建

官方公开归档：

<https://repo.huaweicloud.com/openharmony/os/5.1.0-Release/ohos-sdk-windows_linux-public.tar.gz>

SHA256：`8c1f97df34812e45fa4f0e3275987b261e80622abb7c51392bb063f853451344`。

将 Linux native/toolchains/previewer 归档解压到 `/opt/erika-ohos-sdk-5.1.0/linux`，然后运行：

```sh
bash /review-logs/ohos-native-build.sh
```

此命令针对 API 18 构建完整修改后的 ARM64 Flutter 和 ArkTS 桥接，链接 SDK 库及按校验和固定的 v0.2.1 C API Release。它不重建本分支 Rust runtime，也不运行模拟器。

## OHOS 模拟器：实际生产运行库

上述仅 SDK 命令是初步 ARM64 链接检查。实际播放需使用官方 DevEco Studio 模拟器、匹配的 x86_64 镜像、本分支 Rust runtime 和完整生产 NAPI 桥接：

```sh
bash /review-logs/ohos-x86_64-deps-build.sh
bash /review-logs/ohos-x86_64-runtime-build.sh
bash /review-logs/ohos-x86_64-bridges-build.sh
```

先将这些脚本及其 CMake 项目目录从本文件夹复制到 `/review-logs`。`ohos-live/setup.ps1` 准备 ArkTS HAP 项目并放置源文件和 fixture。使用 `devecocli build` 构建，以 `hdc install` 将未签名 HAP 安装到官方模拟器，再用 `hdc shell aa start` 启动 `com.aimesoft.erika.review/EntryAbility`。

应用运行两个 XComponent，各经一个生产桥接，记录音视频播放、暂停、EOF/中间 seek、速率变化、实际 NativeWindow detach/attach、MP3 输出、64 次创建/销毁及旧 ID 拒绝。将 filesDir 中两份 JSONL 导出为 `ohos-arkts-review.jsonl` 和 `ohos-flutter-review.jsonl`，运行 `python summarize-ohos-live.py /path/to/output`，并检查 hdc 截图。

也可运行收集器，等待两个桥接并检查汇总：

```powershell
./collect-ohos-live.ps1 -Hdc C:\path\to\hdc.exe -OutputDirectory C:\path\to\output -ApiVersion 18 -OperatingSystem OpenHarmony-5.1.0.105
```

失败或超时时收集器会保留运行中的模拟器。当前宿主在暂停稳定后采样并比较第二个样本，再单独检查暂停状态下的 EOF seek。不含 `pause-settled` 的旧 JSONL 不满足此检查。

Windows 音频测试需安装 `numpy` 和 `soundcard`，然后以相同的 DLL/媒体布局运行 `python wasapi-loopback.py C:\path\to\Erika`。它从真实默认 WASAPI 输出录制两秒，要求非零 PCM 和实际音频消费，并保存 `merge-wasapi-loopback.wav`。

版本、二进制、结果、失败尝试和覆盖边界见 [鸿蒙模拟器记录](../ohos-runtime.zh.md) 和 [集成验证](../merge-validation.zh.md)。这是 ArkTS 应用中生产 NAPI 的执行，不运行完整 Flutter 引擎，也不证明物理设备零拷贝性能。
