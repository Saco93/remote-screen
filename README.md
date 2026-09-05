# Remote Screen

Rust 原生 Miracast 投屏工具，默认将笔记本内建屏幕以 **HEVC Main 1920×1080@60 + AAC** 镜像到 LG C9。使用电视 Screen Share；无需电视浏览器。

## 构建与使用

```bash
cargo build --release --locked --bins
tools/install-pairing-service
./target/release/remote-screen mirror
```

本机默认持久配对依赖 [NetworkManager 兼容补丁](patches/networkmanager/README.md)。仓库提供补丁源码；新机器需要先按其说明核对和构建插件。系统级安装仍需管理员权限。

`remote-screen` 是工作目录中的发布版副本（不纳入 Git）；重新构建后可直接执行 `target/release/remote-screen`，或使用 `cargo run --release -- mirror`。默认命令就是 `mirror`，无需额外指定 HEVC 参数。

电视打开 Screen Share 后，在系统共享对话框选择内建屏幕 `eDP-1`，首次配对时接受电视连接请求。持久配对路径已在本机完成两次旧配对重连及一次网络服务重启后的磁盘恢复，三次均进入播放且未重新进行 WPS。鼠标包含在画面中，完整保留 eDP 原始比例，不拉伸、不裁切：当前 2880×1800（16:10）桌面缩放为 1728×1080，电视的 1920×1080 画面左右各补 96 像素黑边。笔记本分辨率和显示比例不变。收到电视 PLAY 后，程序自动将默认音频输出与现有播放应用路由到本次 LG 输出。Ctrl+C 停止；程序恢复原音频路由、关闭共享会话并释放本次 Wi-Fi Direct 连接。

```bash
./remote-screen discover
./remote-screen doctor
./remote-screen probe-codecs
./remote-screen mirror --no-audio
./remote-screen mirror --profile rhp2
./remote-screen mirror --profile baseline --mode 720p60 --encoder x264
```

| 参数 | 默认值 | 可选值／含义 |
|---|---|---|
| `--profile` | `hevc` | `hevc`、`rhp2`、`baseline`；兼容旧名 `hevc-experimental` |
| `--mode` | `1080p60` | `1080p60`、`1080p30`、`720p60`；HEVC/RHP2 当前限定 1080p60 |
| `--encoder` | `auto` | 自动优先 VA 硬件；`vaapi`、`x264`；HEVC 要求 VA |
| `--tv` | `OLED65C9` | 设备名称子串或完整 P2P MAC，须唯一匹配 |
| `--scan-seconds` | `8` | 发现时长，1–60 秒 |
| `--timeout` | `120` | 授权、网络、协议各阶段的超时上限，1–600 秒 |
| `--state-dir` | `.state` | 日志和会话锁目录 |

`probe-codecs` 查询电视原始视频、音频能力并结束连接，不共享屏幕。日志为 `.state/miracast-rust.log`，权限 0600；另一个会话持有锁时拒绝启动。

## 持久配对与连接恢复

本机 NetworkManager 1.58.1-1 会删除由外部服务创建且无人持有的 P2P 接口，因此还安装了仅修改 Wi-Fi 插件的兼容补丁。`sudo tools/install-nm-p2p-plugin` 校验版本和 SHA256、备份原插件并重启 NetworkManager；`sudo tools/install-nm-p2p-plugin restore` 恢复原插件并重启网络。备份位于 `/var/lib/remote-screen/nm-plugin-backup-1.58.1-1.so`。系统升级可能覆盖此插件，其他版本必须重新核对兼容性，不能直接沿用二进制。补丁和构建说明见 [patches/networkmanager](patches/networkmanager/README.md)。NetworkManager 主程序保持系统版本。

`tools/install-pairing-service` 安装 root 所有的辅助二进制和 systemd socket 服务，需要一次管理员认证。`/run/remote-screen-p2p.sock` 只允许安装时的桌面用户连接；桌面采集、音频和编码仍以普通用户运行。配对凭据保存在 `/var/lib/remote-screen/pairings`，目录0700、文件0600，不写入普通日志。

后续连接优先重新邀请已保存的持久组。退出只拆除本次活动组，不删除配对。初始 RTSP 连接超时后，主程序保留同一次 Portal 授权并尝试一次旧配对重连；恢复时禁止自动重新 WPS。已有配对被电视拒绝时会报告错误，不擅自覆盖凭据。`--legacy-pairing` 可显式使用原 NetworkManager 路径排障，该路径仍可能反复要求电视确认。

本机辅助服务暂限定 `wlo1` 及上述 C9 的协商 IP 范围。当前版本通过 58 项 Rust 测试和 Clippy。实机日志已验证普通重连和 NetworkManager 重启后的磁盘恢复，均复用旧配对进入播放；尚未验证电视断电或整机重启，电视端是否另有 Screen Share 提示需以实际观察为准。

## 实现和系统依赖

项目源码和协议、设备管理、媒体配置、音频路由、诊断测试均用 Rust 编写，不再启动 Python、GNOME Network Displays、ffmpeg 或 gst-launch 子进程。系统 GStreamer、PipeWire 和 PulseAudio 兼容服务仍由 Rust 绑定调用；硬件编码通过 GStreamer VA 插件使用系统 VA-API 驱动。这不是完全不含 C 系统库的静态程序。

已验证环境：Arch/Omarchy、Hyprland、Rust 1.98、GStreamer 1.28.6、Intel Arrow Lake GPU。构建需要 Cargo、pkg-config、GStreamer 与 PulseAudio 开发库。运行需要 NetworkManager + Wi-Fi Direct、XDG Desktop Portal ScreenCast、PipeWire/PulseAudio 兼容服务，以及以下 GStreamer 元件：

- 采集：`pipewiresrc`、`pulsesrc`。
- 编码：`vah265enc`（默认）；`vah264enc`／`x264enc`（H.264）；`fdkaacenc` 或 `avenc_aac`。
- 封装：`h265parse`、`h264parse`、`aacparse`、`mpegtsmux`、`rtpmp2tpay`、`rtpbin`。

`doctor` 检查本机元件和内建屏幕。程序直接通过 D-Bus 使用 NetworkManager 和桌面共享 Portal，通过 Hyprland IPC 识别内建屏幕。默认通过本工具的 root 辅助服务管理 Wi-Fi Direct 持久配对，NetworkManager 只负责发现。服务根据 supplicant 协商的 IP 字段确认 GO 为 192.168.49.1、本机为 192.168.49.10/24 后配置组接口，不添加默认路由；不支持的地址明确报错。旧 `--legacy-pairing` 路径仍由 NetworkManager 使用 DHCP。程序拒绝覆盖其他正在使用的 P2P 会话。

电视支持主动请求恢复关键帧时，VA 硬件编码使用当前插件允许的最大间隔 1024 帧（60fps 时约 17.1 秒），保留按需 IDR；不支持主动请求时每秒发送关键帧。`key-int-max=0` 表示自动选择，不能关闭周期关键帧。RTP 在当前已就绪的一批数据内合并，每包最多 7 段 TS（1328 字节），不足整包的尾部立即发送，不等待后续帧。HEVC 使用 VCM（视频会议）码率控制，目标 4096kbps，减少大关键帧的突发发送；它不是瞬时流量硬上限。

当前电视为 `192.168.49.1`，DHCP 分配本机 `192.168.49.10`。沿用此前已批准的防火墙规则，不在程序中自动更改系统规则：

```bash
sudo ufw allow in on p2p-wlo1+ proto tcp from 192.168.49.1 to 192.168.49.10 port 7236 comment 'LG C9 Miracast'
```

地址不符合当前规则时程序会明确报错。RTP/RTCP 源端口为 50000/50001，目标端口由电视协商。

## 验证

```bash
cargo test --locked
cargo clippy --all-targets -- -D warnings
./remote-screen verify-media
./remote-screen verify-media --profile rhp2
./remote-screen verify-media --profile baseline --mode 720p60 --encoder x264
```

`verify-media` 使用合成动态画面和声音，通过真正的编码、MPEG-TS、RTP/RTCP 管线发往本机 UDP。检查帧输出、包长、视频／AAC PES、PMT 编码类型、最长 GOP 和主动请求 IDR。需要 GPU 和本机网络权限，不连接电视；默认 5 秒，`--test-seconds` 可调整。

独立统计外发帧率（接口序号以本次连接输出为准，需要原始 socket 权限）：

```bash
sudo ./remote-screen measure --interface p2p-wlo1-27 --seconds 10
```

只统计发给 LG 的 RTP 视频时间戳，不保存屏幕内容。发送帧率不能代替电视端显示帧率或端到端延迟。

2026-09-05 Rust 实机验证：HEVC Main Level 4.1 1080p60，电视接受 M4、SETUP、PLAY，持续响应保活；独立测得 601 帧／9.984 秒（时间戳帧率 60.00fps）。音频源连接到新建的 LG 输出，退出后正确恢复音频、释放 Portal 和 P2P。用户已确认此前 HEVC 模式画面、声音和总体体验正常；Rust 版的现场主观反馈单独确认。

最终验证：42 项 Rust 测试全部通过，`cargo clippy --all-targets -- -D warnings` 与 `cargo fmt --check` 通过。正式版 Rust `measure` 命令再次测得时间戳帧率 60.00fps（606 帧／9.984 秒到达区间）。协议测试覆盖消息分片、完整模拟电视协商、拒绝配置、能力位和取消操作。旧 Python/C 实现归档于本机 `.state/legacy-python-c.tar.gz`，不参与构建或运行。协议调查与早期实验见 [RESEARCH.md](RESEARCH.md)。

2026-09-05 延迟优化：修正 PipeWire 启动时的非实时状态报告，避免 VA 编码器额外缓存 4 帧；`videorate drop-only=true` 不等待下一帧或复制帧，保留输入时间戳；UDP 保持时钟同步，移除额外 20ms processing deadline。桌面实际帧率可随更新频率下降，上限为所选模式帧率。HEVC、AAC、等比例缩放和黑边保持不变。早期外发 RTP 时间戳年龄记录为 86.9ms → 49.0ms；后续配对测量发现 MPEG-TS 外层时间戳存在起点偏移，因此不能把这些数值解释为采集到发包的实际延迟，也不能据此声称实际减少 38ms。动画采样 10 秒获得 587 帧。完整修正及逐帧测量见 RESEARCH.md。

需要定位延迟时可运行 `REMOTE_SCREEN_TRACE_LATENCY=1 ./remote-screen mirror`。默认关闭诊断；启用后仅保留有界的数值统计，不保存屏幕内容。配对计时按同一帧的采集到达时间和视频 PES PTS 关联各阶段。日志中的 `network.sink` 位于发送调度之前，不能作为实际外发或电视显示时间。

后续逐帧实测：默认将缩放及颜色转换各设为2线程，采集到UDP发送器入口的稳定窗口中位数从约14–15ms降到10–11ms；p95约18–20ms，尚未证明尾部延迟改善。该口径排除UDP时钟等待、无线传输和电视解码显示；声音已恢复，音频对照未发现显著视频等待。

HEVC 色彩：输入明确采用 BT.709 有限范围 NV12；Rust 在编码器输出、h265parse 缓存 SPS 之前补齐色彩描述（原色、传递曲线、矩阵均为 BT.709）。当前 GStreamer VA 编码器虽在 caps 标为 BT.709，却未把描述写入 HEVC SPS。修正只改参数集，不重编码视频，保留时间戳；合成码流的修正前后解码像素完全相同。它针对当前已验证的 VA Main SPS；遇到未支持的复杂 SPS 结构明确报错，不猜测比特布局。电视图像设置仍可能影响与 HDMI 的观感差异。

PCR 播放预留默认采用50ms：`./remote-screen mirror` 等同于显式指定 `--pcr-lead-ms 50`。它将 MPEG-TS 原有125ms预留缩短为50ms（PCR提前75ms），不改PES时间戳、音视频相对同步或编码内容。参数范围0–125ms；恢复原始mux行为请显式指定 `--pcr-lead-ms 125`。`verify-media` 会校验实际输出的PES/PCR时差。

2026-09-05 相机对照：0ms与50ms的清晰样本屏间时差中位数分别约121ms和126ms，未证明0ms具有稳定优势；用户选择保留50ms。测量包含拍摄和屏幕刷新误差，不是完整输入到显示延迟。0ms仍可通过 `--pcr-lead-ms 0` 试验，不代表端到端零延迟。

## License

项目代码采用 [MIT License](LICENSE)，与 [Saco93/voice-input](https://github.com/Saco93/voice-input) 一致。NetworkManager 补丁采用 LGPL-2.1-or-later，详见 [补丁说明](patches/networkmanager/README.md)。
