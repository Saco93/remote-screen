# LG C9 native screen mirroring

调查与现场验证日期：2026-09-05。

> 当前版本已完整迁移为 Rust，默认 HEVC 1080p60。下文 GNOME、Python、C 补丁和早期缓冲测试为迁移前的历史研究记录，不代表当前构建方式。旧文件已归档于 `.state/legacy-python-c.tar.gz`；当前用法与验证见 README.md。

## Rust 迁移验证

Rust 通过 zbus 直接管理 NetworkManager 和 ScreenCast Portal，自行处理 WFD RTSP，通过 Rust GStreamer 绑定构造 HEVC/AAC/TS/RTP 管线。删除了原 Python 控制层和 C 后端依赖。实际传输保留 HEVC Main Level 4.1、无 B 帧、NV12、4096kbps、约 50ms CPB、单帧可丢弃编码前队列、媒体系统时钟、PipeWire 100ms 静态刷新、AAC48k双声道。

首次 Rust 连接使用静态 IPv4，虽 P2P 已激活，电视没有发起 RTSP。对照旧 C 实现，将 IPv4 改回 DHCP、IPv6 自动、两者禁止默认路由后，电视立即完成 M1–M7。此处表明地址已配置不等于电视已完成连接流程。

Rust 实机输出 caps 为 HEVC Main Level 4.1、1920×1080@60；外发统计 601 帧／9.984s，时间戳帧率60.00fps。电视连续响应保活。Ctrl+C 后音频输出和捕获源消失，应用回到原音频输出，P2P 断开，普通 Wi-Fi 保持连接。未通过发送端数据推断额外的电视显示延迟改善。

用户已确认先前 HEVC 投屏“总体效果还是不错的，其他一切正常”，因此 HEVC 成为首选默认；它仍未出现在电视返回的能力位中。

LG C9 支持 Miracast／Screen Share、AirPlay 2 和 DLNA。本项目采用 Miracast 原生屏幕镜像。此前实现的浏览器 HLS 播放不符合用户要求，已从工具及使用文档中移除。

## 当前系统与电视

- Omarchy/Arch，Hyprland；内建屏幕 `eDP-1`，Samsung Display，2880×1800，缩放 2。
- GNOME Network Displays `0.99.0.r5.g88fc480-1`；源码 HEAD `88fc480220f156af56008f134f2bbb1d4a071406`。
- NetworkManager 有 `p2p-dev-wlo1`；实测发现 `[LG] webOS TV OLED65C9PCA`。
- 电视 WFD 设备信息为 `00 00 06 01 11 1c 44 00 32`，表明它是 Miracast 接收端。
- 电视原生应用 `com.webos.app.miracast`，标题 Screen Share，描述 Miracast / Intel WiDi。通过已配对控制接口成功启动。

## 实测进展与限制

11:58:58 后端报告创建屏幕共享 session，检测到 x264enc、fdkaacenc、mpegtsmux。随后进入 `ND_SINK_STATE_WAIT_P2P` 和 `ND_SINK_STATE_WAIT_SOCKET`。

11:59:03 wpa_supplicant 报告 `P2P-GROUP-STARTED`，电视为 group owner，地址 `192.168.49.1`；笔记本 P2P 地址 `192.168.49.10/24`。后端监听 TCP 7236。进入 WAIT_SOCKET 尚不表示电视已请求视频，只有 RTSP PLAY 后的 `ND_SINK_STATE_STREAMING` 才说明传输开始；最终画面仍需现场确认。

首次连接在 11:58:16 出现 Sink error，随后进程 PID 503990 以 SIGABRT 退出，错误为 `double free or corruption (out)`。core 及二进制反汇编表明崩溃发生在 PulseAudio cleanup 的 `pa_context_disconnect`；另一个音频主循环线程仍在运行。源码先释放 context，再停止 threaded mainloop，且缺乏相应加锁。这是连接错误后的次生崩溃，不能解释原始连接失败；未修改系统发送端。

12:01:36 内核明确记录 `[UFW BLOCK] IN=p2p-wlo1-2 SRC=192.168.49.1 DST=192.168.49.10 PROTO=TCP DPT=7236 SYN`。12:01:40 电视结束 P2P 会话，发送端进入 ERROR 并再次在清理阶段崩溃。该证据确认原生 RTSP 控制连接被防火墙阻挡。

用户随后明确批准，已成功添加只在 `p2p-wlo1+` 接口上放行电视 `192.168.49.1` 至笔记本 `192.168.49.10:7236/tcp` 的规则，UFW 返回 `Rule added`。此前浏览器端口 8765 的规则没有添加。放行后已成功连接并协商 1920×1080@30。

## 一手来源

- [LG C9 产品规格：Miracast、DLNA、webOS 4.5](https://www.lg.com/it/tv-soundbar/oled/oled55c9pla-tv-oled/)
- [LG C9 产品规格：Miracast、AirPlay 2](https://www.lg.com/nz/tv-soundbars/oled/oled55c9pva/)
- [LG AirPlay 2 公告：包含 C9](https://www.lg.com/ch_de/ueber-lg/presse-medien/lg-integriert-apple-airplay2-und-unterstuetzt-homekit/)
- [GNOME Network Displays 上游](https://gitlab.gnome.org/GNOME/gnome-network-displays)
- [上游 README 镜像：Miracast、LG webOS 和 P2P 条件](https://github.com/GNOME/gnome-network-displays)
- [Hyprland 屏幕共享](https://wiki.hypr.land/Useful-Utilities/Screen-Sharing/)

当前安装版本的原生 stream URI、协议枚举、RTSP 状态由本机包对应源码 `src/nd-wfd-p2p-sink.c`、`src/nd-sink.h` 和 `src/stream/nd-stream.c` 核对，避免将旧 README 的界面行为当作当前实现。


## 本地原生发送端修复与验证

系统安装未修改。`scripts/build-native-backend` 固定上述 upstream revision，将源码导出到 `.state/native-build` 并应用 `patches/`：

1. PulseAudio 先停止线程再释放 context；将 READY 分支的自动释放变量限制在独立作用域。修复后真机错误退出码为 0，未再 SIGABRT。
2. 添加 videorate 和方形像素输出，适配 Hyprland 0/1 可变帧率与 120Hz 屏幕。
3. x264 在初始化阶段设置 zerolatency/veryfast。原编码器预读 64 帧，有限的屏幕 buffer 导致首包迟迟不能输出；本地实验确认四帧输入不发 EOS 时默认编码器三秒内无输出，修补后输出首包。
4. 排障阶段使用原生仅画面模式，协商 `wfd_audio_codecs: none` 并不创建音频管线。此前真机日志出现音频 PTS 从约 7.5 秒跳到 12.6 秒，随后复用等待；仅画面模式避免依赖该音频时钟。
5. PipeWire 每 100ms 刷新静态帧，使静态桌面也能响应关键帧请求。
6. Portal source 设置 `provide-clock=false`、保留 `do-timestamp=true`，视频源使用系统时钟，避免重协商中的 PipeWire 时钟影响整个媒体管线。

`python scripts/verify-native-video.py` 验证 120fps 和 0/1 max120 输入均输出约30帧/秒、33.333ms PTS 间隔、1920×1080、PAR1/1、16:10内容左右黑边，以及四帧输入即可输出首个 H.264 包。

12:13:18 仅画面模式真机收到 RTSP PLAY，进入 STREAMING。后续观察到 P2P 发送字节数增长和 RTSP ESTAB；实际电视画面仍等待用户确认。

12:16:15 时钟修正后的真机再次进入 STREAMING；12:16:18 至 12:16:27，P2P 发送计数从 273634 增至 651593 byte。这证明实际发送数据，但不能代替电视画面确认。

最终真机观察：12:16:15.939 进入 STREAMING；12:17:16 P2P 发送计数为 6224256 byte，RTSP 在观察期间保持 ESTAB，12:16:40 和 12:17:05 正常发送 keep-alive。12:17:23.838 主动终止测试后正常 DISCONNECTED，工具退出码 0；约 68 秒会话未再次自行断开。电视实际画面的清晰度、流畅度和延迟没有用户确认，因此不作为已验证结论。


## 操作延迟和音频修复

用户确认电视有画面，但操作延迟偏高，切换 LG 音频输出后无声。此前默认关闭音频是无声的直接原因，现已恢复默认音视频传输，保留 `--no-audio` 作为显式选项。

- 补丁 0008：除 OpenH264 的兼容路径外，不再强制 500ms 管线延迟，使用 GStreamer 自动协商；编码前单帧队列丢弃过期原始帧，不丢弃压缩帧。
- 补丁 0009：PulseAudio source 设置 `provide-clock=false`，与视频共用系统时钟；音频缓冲从默认 200ms 改为 40ms，采集周期 10ms。
- `scripts/verify-native-latency.py` 独立编译实际 WFD factory，以 1080p30 测试源经过 x264、MPEG-TS、RTP 和同步 sink，测量同一 segment 时间基准下的输出延迟。旧配置约 500.2ms，新配置两次运行约 171.0ms 和 175.5ms；该测试不含音频源、网络或电视显示。
- 12:20:21.084 真机会话进入 STREAMING，日志确认音频编码已选择并创建音频源；PulseAudio source-output 捕获 LG 虚拟输出 monitor。向该输出发送两秒测试音，paplay 正常退出；随后观察到原有播放应用也已路由到 LG 输出。12:22 左右 P2P 累计发送超过 11MB，RTSP 持续保活。以上证明发送端音频路径已启用，不能代替电视扬声器实际出声和主观延迟确认。


## 第二轮鼠标延迟优化

用户确认电视音频已正常，但鼠标延迟仍可感知。保持当前 1080p30 清晰度，新增补丁 0010：MPEG-TS mux alignment=0，立即交出现有 TS packet；RTP payloader max-ptime=1ms，避免等待多个画面时间戳的数据凑满 MTU。音视频继续共用系统时钟。

`python scripts/verify-native-latency.py` 直接编译真实 factory，并进行四组同步输出实验：原固定缓冲 500.2ms、上轮自动协商及 MTU 聚合 179.7ms、本轮及时发包 98.0ms、本轮含 AAC 95.6ms。检查到 44 个视频 PES 和 73 个 AAC PES，首尾音视频 PTS 差分别为 31.9ms 与 -37.5ms；RTP payload 都是 188 字节整数倍，总包不超过 1400 字节。数值为本地合成媒体输入至同步输出的测量，不能作为电视鼠标端到端延迟。

现场 P2P 连接使用 2.4GHz 信道 1，而普通 Wi-Fi 使用 5GHz 信道 36。P2P 接口省电原为 on；临时执行 `pkexec iw dev p2p-wlo1-11 set power_save off` 后，两次 30 包 ping 均值约 10.3ms、最大约 39–41ms，修改前 12 包均值 26.1ms、最大 58.3ms。这是短样本观察，不足以证明全部改善来自省电设置。没有修改系统持久配置，临时设置随旧 P2P 接口销毁而失效。

新版第一次重连停在 WAIT_SOCKET 后正常错误退出；重新打开电视原生 Screen Share 后再次连接，12:28:17.227 收到 RTSP PLAY 并进入 STREAMING，日志确认 AAC 已选择。12:28:41 正常保活，随后 P2P 发送计数从 3,415,292 增至 6,455,332 字节。已将重连前使用 LG 输出的应用和默认输出恢复到新 LG sink；source-output 正在捕获该 sink monitor。当前接口 p2p-wlo1-13 也临时关闭省电，读取确认 off。会话保持运行，供用户实际体验；新版的电视端主观响应尚未确认。


## 可选 60fps 模式

新增 `--mode 720p60` 和补丁 0011。后端检查所选 codec 的支持列表，只在明确包含 1280×720@60 progressive 时选择；否则警告并保留 1080p30。默认仍为 1080p30，终端现在显示实际选择的分辨率与帧率。LG C9 的现场能力日志明确列出 720p60；1080p60 只出现在 native 字段而未列入支持列表，因此此次未提供该模式。

构建通过。`python scripts/verify-native-video.py` 确认固定 120fps 和可变帧率输入均可输出 720p60，61 帧、PTS 间隔 16.667ms，保持比例及左右黑边。`python scripts/verify-native-latency.py --mode 720p60` 使用实际 factory 完成 x264/MPEG-TS/RTP 联合测试，本地视频输出约 53.5ms，加 AAC 约 53.6ms；解析 89 个视频 PES、73 个 AAC PES，起始/末尾 PTS 差 32.2ms/-20.5ms，数据包检查通过。当前电视会话未切换，因此尚未验证真机 720p60 播放或其端到端延迟。


## 原始能力核对及 1080p60 实验

12:41:02.710 重新握手取得原始 `wfd_video_formats`：

```text
40 00 01 10 000194FF 155575DF 00000555 00 0000 0000 1F none none, 02 10 000194FF 155575DF 00000555 00 0000 0000 1F none none
```

两个 codec 的 native 均为 0x40，CEA mask 均为 0x000194FF。1080p60 的 CEA bit8（0x100）确实为0；该差异不是本地解析丢失。profile分别为baseline/high，level位0x10。`scripts/verify-native-capabilities.py` 独立编译真实codec源码，验证合成的置位/清位案例，再解析上述真实raw，两种profile均确认bit8 clear、native1080p60。

一手交叉核对：[AOSP VideoFormats.cpp](https://android.googlesource.com/platform/frameworks/av/+/1b92868/media/libstagefright/wifi-display/VideoFormats.cpp)，CEA index8为1080p60，native=(index<<3)|type，因此CEA类型native=0x40；M4所选CEA字段应为00000100。

添加0012原始能力日志、0013显式实验模式`--mode 1080p60`及发送descriptor日志、0014一次性180帧编码输出审计。1080p60只在能力位或native字段支持时尝试；仅native时发出警告。默认1080p30保持不变。构建和5项单元测试通过；实际factory的1080p60联合媒体测试输出约53.8ms，音视频PTS首尾差15.6ms/-37.1ms，完整TS/RTP包检查通过。该数值不包括网络或电视显示延迟。


### 1080p60 真机结果

12:44:05.772 再次收到相同raw，选择1920×1080@60。发给电视的M4为 `00 00 01 10 00000100 00000000 00000000 00 0000 0000 01 none none`；12:44:05.776 SET_PARAMS完成，12:44:06.475电视发出PLAY并进入STREAMING。

编码器实际caps为1920×1080、framerate60/1、progressive、constrained-baseline、H.264 level4.2，与原始level位0x10匹配。首180帧（包含启动）按墙钟48.71fps、按PTS56.53fps。为排除启动影响，新增 `scripts/measure-wfd-framerate.py`，只在指定P2P接口读取发往电视UDP53000的RTP/TS头，统计视频PID4113的唯一PES PTS，不保存抓取的数据。对正在运行的p2p-wlo1-17执行10秒测量，得到571个唯一帧，首末间隔9.979秒，墙钟57.12fps、时间戳56.81fps。该结果说明当前实际发包接近但未满60fps，不能代替电视实际呈现帧率或端到端延迟。

12:45:20仍正常保活，P2P发送超过47MB；播放应用及默认输出已恢复到新LG sink，source-output捕获该sink monitor。会话保持1080p60运行，电视画面及声音的本次现场确认仍待用户回复。原始能力位和实际接收行为并不完全一致，因此1080p60保留为显式实验模式，默认仍1080p30。


## Intel 硬件 H.264 接入

用户同意继续接入后，安装与当前GStreamer一致的gst-plugin-va 1.28.6-2，未升级其他包；已有intel-media-driver和/dev/dri/renderD128。GStreamer识别vah264enc，1080p60动态ball的120帧冒烟测试通过。0015补丁为VA配置b-frames=0、b-pyramid=false、ref-frames=1、target-usage=7、CBR、AUD以及约50ms目标CPB；新增--encoder auto/x264/vaapi，auto沿用上游硬件优先选择顺序。

`scripts/compare-native-encoders.py` 独立编译真实factory，使用1080p60动态画面+AAC、同4096kbps目标并禁止B帧，统计同步RTP延迟和进程CPU时间；两种编码均检查239个视频PES与189个AAC PES、PTS单调、音视频首尾差<50ms，以及完整TS/RTP包与MTU。根agent复测：x264平均53.6ms、p95 53.8ms、CPU4.348s/墙4.071s（106.8%）；VA平均53.6ms、p95 53.8ms、CPU3.331s/墙4.073s（81.8%）。硬件降低CPU约23%，未降低本地发送延迟；另一次完整验证降低CPU约19%。CPU包含源生成、转换、音频编码与复用；延迟不包括portal、无线或电视显示。相同码率目标不意味着实际输出字节数或画质完全相同。


### 硬件编码真机验证

前两次重连停在WAIT_SOCKET，尚未进入编码协商；读取电视前台为HDMI3且临时P2P接口无IPv4地址。重新打开原生Screen Share后，12:53:59.832成功选择1080p60，12:54:00.534收到PLAY并进入STREAMING。后端日志确认选用vah264enc与fdkaacenc。实际编码caps为1920×1080、60/1、progressive、constrained-baseline、Level4.2。

稳态10秒包头统计：595个唯一视频帧，首末间隔9.978秒，墙钟59.53fps、PTS59.70fps。此前x264真机测量约57fps，但两次桌面内容与运行时间不同，因此不能视为严格受控性能对比。12:54:50继续正常RTSP保活，P2P发送超过47MB；默认输出与原播放应用已恢复到当前LG sink，音频source-output正常捕获该sink monitor。会话保持运行，当前电视显示帧率和主观延迟未测量，未声称硬件编码已经降低端到端延迟。


## 用户确认及扩展编码探测

用户明确确认：Intel硬件H.264投屏的操作延迟明显降低，视频流畅度提高。这是用户现场体验结论；此前合成媒体测试仅证明CPU下降，不能替代此主观反馈或量化端到端延迟。

`probe-codecs`新增独立M3探测，收到回复即退出，不发送M4。12:59:08.482电视返回200，除legacy外也返回wfdx_video_formats及wfd2_video_formats。MS扩展仅profile0001/0002；R2六个tuple均codec01（H.264），profile01/02/04/08/10/20，未出现HEVC codec02。完整原文保存在.state/extended-codecs-probe.log。发送端vah265enc已通过120帧1080p60动态编码测试，但当前电视扩展能力没有提供可协商HEVC的条目。

R2 profile04为RHP2，相比RHP可启用CABAC并保持无B帧。开始实现显式`--profile rhp2`（要求1080p60/vaapi），使用R2 source IE、wfd2_video_formats和wfd2_audio_codecs协商，保留现有Baseline模式。


### RHP2 实现与验证

0017使用R2 source subelement `0b 00 02 00 00`、解析电视R2 codec01/profile04能力，并要求CEA bit8与至少H.264 Level4.2。发出的M4视频为 `wfd2_video_formats: 00 01 04 0010 000000000100 000000000000 000000000000 00 0000 0000 00 00`；编码器启用CABAC，保留dct8x8=false、B0、ref1。此处SPS为High，协议profile为RHP2，不能把两者数值混用。

0018/0019适配R2音频。首次只查询wfd2_audio_codecs未选出AAC；第二次同时查询两种字段，13:05:58.317实际回复只有 `wfd_audio_codecs: LPCM 00000003 00, AAC 00000001 00`，以及R2视频字段。解析此AAC能力后，以R2音频字段发送选择，13:05:58.983收到PLAY，音频编码确认selected=yes。播放应用及默认输出已恢复到新LG sink。

13:05:58的R2身份握手仍列出6个H.264 tuple，没有HEVC。R2扩展CEA含bit8，说明扩展协议已经明确声明1080p60，先前legacy能力位未列出不代表扩展能力缺失。

R2解析测试通过：真实6tuple响应接受；HEVC-only、none、过低level、无bit8、非RHP2、多个level位、保留level位、截断响应均拒绝。同样1080p60/4096kbps/AAC的两动态场景四组比较均通过TS/RTP、MTU、SPS及音视频时间戳验证。根agent复测：移动球Baseline/RHP2平均延迟53.7/53.7ms，CPU81.0/86.9%；SMPTE动态噪声53.9/53.9ms，CPU64.7/74.4%。另一次比较CPU结果不同，因此不据此断言CPU收益。合成测试没有证明画质、压缩效率或端到端延迟提升。

构建脚本现在从固定upstream导出临时源码并顺序应用整个补丁集，通过后只复制内容变化的文件到生成源码目录，解决重叠补丁的重复构建检查问题；重复构建成功，无需重新编译未变化文件。

协议依据：[Wi-Fi Alliance v2.1 Table6/7/77](https://tools.barco.com/kb-downloads/4814/Wi-Fi_Display_Technical_Specification_v2.pdf)及[MS-WFDPE wfdx_video_formats](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-wfdpe/584aadfe-f0db-4dec-9612-6a1d7bae1b77)。

RHP2稳态包头统计：10秒内595个唯一视频帧，首末间隔9.971秒，墙钟59.57fps、PTS59.60fps。当前会话保持运行；本轮电视主观体验与实际出声仍待用户确认。


## Rust 每秒卡顿回归（2026-09-05）

用户反馈重写后画面约每秒停顿一次。对照旧版发现：电视的 `wfd_idr_request_capability: 1` 原本令 GOP 为 `10 × fps`（600帧），Rust 重写时却固定为 `fps`（60帧）。12秒真实外发采样观察到12个周期关键帧，间隔约1秒；关键帧约208–370KB，发送占用124–274ms，随后的视频帧出现相应间隔；捕获中没有RTP序号缺口。这说明平均60fps不足以证明发送平滑。

修复保留M3协商出的IDR请求能力，在支持时恢复600帧周期，缺失/禁用时维持60帧，并继续处理电视的主动IDR请求。协议完整握手测试覆盖能力1/0/缺失以及实际IDR事件，媒体验证检查真实编码器的`key-int-max`属性。保持HEVC1080p60、码率、音频、媒体时钟和网络设置不变，以单独验证这项回归。

修复后真实会话日志确认 `600 frames; receiver IDR requests=true`，电视接受协商并进入PLAY。22秒外发采样包含1319个视频PES，无RTP序号缺口，仅2个周期关键帧，间隔9.964秒；但它们仍占用约268/311ms的发送时间。因此已证实每秒关键帧回归消除，尚未证明所有可感知停顿消失。23个单元测试、release构建、fmt、clippy以及实际HEVC1080p60+AAC媒体验证通过。

后续定位：RTP udpsink 的 sync=true 与原后端默认一致，不能作为回归修改。当前P2P使用2412MHz，报告发送速率72.2Mbps、信号约-67至-71dBm；这些统计本身不证明无线是瓶颈。独立SMPTE+AAC本机回环诊断中，359帧最大相邻帧发送起始间隔22.12ms，约48–132KB关键帧发送1.55–4.80ms。回环采样有168处RTP序号缺口，且内容与真实桌面不同，故只能作为未复现长停顿的线索，不能作等负载性能结论或端到端延迟测量。剩余约10秒关键帧发送停顿仍待定位；当前保留已修复会话供用户观察。

## 关键帧发送停顿与频率优化（2026-09-05）

本轮起始会话外发5秒共15030个RTP包，全部200字节（188字节TS+12字节RTP头）。先改成7段TS分组：真实会话采样全部1328字节，38秒内两个约305/306KB关键帧发送52.42/77.17ms，间隔17.067秒，无RTP序号缺口。此前600帧周期采样约268/311ms；两轮桌面内容未严格固定，因此这些是现场观测，不是等内容压缩效率基准。

本机GStreamer1.28.6的VA H.264/H.265 `key-int-max`上限1024，0表示自动；HEVC源码也将更大值夹到1024。支持接收端IDR请求时现使用最大间隔1024（60fps约17.07秒），保留主动IDR；不支持时保持fps帧间隔。x264采用其允许的最大值2147483647，场景切换仍可能提前生成关键帧。[HEVC GOP源码](https://github.com/GStreamer/gstreamer/blob/1.28.6/subprojects/gst-plugins-bad/sys/va/gstvah265enc.c#L3799)。

纠正早期CPB解释：HEVC插件会把过小的204Kb CPB自动提高到8192Kb（约2秒），原先约50ms的设置没有生效。[源码](https://github.com/GStreamer/gstreamer/blob/1.28.6/subprojects/gst-plugins-bad/sys/va/gstvah265enc.c#L3222)。同SMPTE180帧1080p60、4096kbps、GOP60硬件对照：CBR的CPB204和2048输出完全相同，IDR分别131971/94228/59062字节，总1861122字节；VCM输出IDR121369/14275/25581字节，总2009826字节。VCM在该样本减小周期IDR，但总数据量增加约8%，并非所有画面都节省流量。VCM不提交HRD参数，因此CPB不能用来约束该模式的单帧大小。[源码](https://github.com/GStreamer/gstreamer/blob/1.28.6/subprojects/gst-plugins-bad/sys/va/gstvabaseenc.c#L1231)。

HEVC切换VCM后的第一轮真机采样38秒：2280帧，无RTP序号缺口；两个关键帧247621/251080字节，发送35.18/39.84ms，间隔17.067秒；相邻视频帧发送起始最大间隔47.71ms。这仍是发送端包头统计，不是电视解码/显示延迟测量。

最终分组不采用固定alignment7等待凑满：Rust将alignment0输出的每批已就绪TS合并后交给RTP按MTU切分，尾包立即发送；只有一个TS时附空buffer触发payloader flush，不产生空RTP。它不跨下一帧缓存。[mux源码](https://github.com/GStreamer/gstreamer/blob/1.28.6/subprojects/gst-plugins-bad/gst/mpegtsmux/gstbasetsmux.c#L1311)、[payloader源码](https://github.com/GStreamer/gstreamer/blob/1.28.6/subprojects/gst-plugins-good/gst/rtp/gstrtpmp2tpay.c#L184)。新增真实UDP测试：只注入1/3/15个TS、不提供后续帧，仍完整收得1/1/3个RTP包，字节和时间戳保留。25项测试、fmt、clippy和release构建通过；实际HEVCMain4.1 1080p60+AAC媒体验证通过，确认长GOP下按需IDR仍生成。

最终即时尾包版本真机验证（p2p-wlo1-33）：38秒2279个视频PES，无RTP序号缺口；两个关键帧251677/307380字节，发送34.18/72.89ms，间隔17.067秒。相邻视频帧发送起始间隔p95为21.64ms、最大72.90ms。该结果比前一轮VCM样本有更大的第二个关键帧，不能把35–40ms当作固定保证；当前已消除此前小包导致的约268–311ms级停顿，但仍有34–73ms的关键帧发送占用。最终会话继续运行，电视正常响应RTSP保活；现场画质、出声与主观操作体验待用户反馈，不把发送端统计等同于端到端延迟。

## eDP 镜像比例修复（2026-09-05）

用户要求完整保留画面、禁止拉伸/裁切，采用方案a：eDP保持2880×1800（16:10），电视1920×1080内使用1728×1080桌面、左右各96像素黑边。

修复前直接在内存解码外发HEVC，左右96列分别91.24%/97.75%的像素亮度高于20，证明发送内容没有黑边。随后实际管线日志确认缩放输入BGRA2880×1800的像素比例为异常的1/2147483647，输出1920×1080为1/1。PipeWire将源与下游caps相交后通用fixate，未指定的PAR范围被固定为极小值；videoscale计算比例溢出，无法计算黑边。[PipeWire源码](https://github.com/PipeWire/pipewire/blob/1.6.8/src/gst/gstpipewiresrc.c#L1298)、[GStreamer源码](https://github.com/GStreamer/gstreamer/blob/1.28.6/subprojects/gst-plugins-base/gst/videoconvertscale/gstvideoconvertscale.c#L921)。

在采集后、videorate之前明确约束pixel-aspect-ratio=1/1，保留等比例videoscale add-borders。新增测试先用宽PAR范围验证协商锁定1/1，再检查真实缩放像素：左右96列全黑、桌面四边不同颜色标记完整存在，防止裁切。26项测试、clippy、fmt、release构建及HEVC1080p60+AAC媒体验证通过。修复后实际管线日志确认输入2880×1800 PAR1/1，输出1920×1080 PAR1/1。

实际发送流复验（p2p-wlo1-36，内存解码两帧，不保存屏幕内容）：左右各96列亮度高于20的比例均为0；左侧亮度0–1、右侧全0，中央保留桌面内容。已证实黑边进入实际HEVC码流。会话保持播放，笔记本显示配置未修改。


## 实时源启动状态与操作延迟优化（2026-09-05）

基线为已修正画面比例的 Rust HEVC VCM 1080p60 + AAC。增加可选 `REMOTE_SCREEN_TRACE_LATENCY`，通过各 pad 的 Segment 将 PTS 映射到 pipeline running time，避免编码器约 1000 小时的时间戳偏移。每阶段只保留 300 个数值样本，通过有界通道在独立线程报告；默认不安装探针。阶段分布不是逐帧配对，不能简单相减求各段耗时；network.sink 在发送调度之前。

根因：PipeWire 1.6.8 初始化时设置 GstBaseSrc 为 live，但自有 is_live 尚为 false。协商先 set_caps，再 stream_start/parse_stream_properties。VA 编码器在 CAPS 初始化中查询上游 latency，此时得到 live=false，HEVC 选择 preferred_output_delay=4；稍后源变为 live=true 不会自动清除已选延迟。60fps 对应 66.7ms。修复仅对真实桌面 capture.src 的上游 latency 查询返回阶段安装探针，把 live=false 改为 true，保留 min/max；日志确认启动时触发校正，编码器查询的 min 从 66.7ms 变为 0。

源码依据：[PipeWire 1.6.8 capture](https://github.com/PipeWire/pipewire/blob/1.6.8/src/gst/gstpipewiresrc.c)、[VA 上游实时性查询](https://github.com/GStreamer/gstreamer/blob/1.28.6/subprojects/gst-plugins-bad/sys/va/gstvabaseenc.c#L904)、[HEVC 输出延迟选择](https://github.com/GStreamer/gstreamer/blob/1.28.6/subprojects/gst-plugins-bad/sys/va/gstvah265enc.c#L4525)。

另外启用 videorate drop-only，输入帧不再等待下一帧，保留实际 PTS，最多输出所选帧率；UDP processing-deadline=0，继续 sync=true 和共同媒体时钟，不引入过期包丢弃。音频仍为 48kHz 双声道 AAC，采集 latency 10ms / buffer 40ms；等比例缩放、左右黑边及关键帧策略保持。

外发统计通过 AF_PACKET 观察到电视 UDP 53000 的 RTP 视频 PES 首包，以当前 pipeline base_time 和单调时钟计算该包 RTP 时间戳的年龄，不保存画面 payload。它测量发送端网络栈出包时的时间戳年龄，不包含无线送达或电视显示。各轮桌面内容未严格固定，最终一轮短暂显示移动球动画；以下是现场采样，不能视作等内容端到端基准。

| 状态 | 查询 pipeline min | 10 秒视频帧数 | 首包年龄 p50 | p95 | 最大值 |
|---|---:|---:|---:|---:|---:|
| 原实现，含音频 | 86.7ms | 599 | 86.87ms | 87.67ms | 90.00ms |
| 原实现，无音频 | 86.7ms | 601 | 86.96ms | 87.36ms | 96.69ms |
| 仅修正实时状态，含音频 | 30ms | 601 | 47.84ms | 58.62ms | 112.27ms |
| 最终版本，含音频、动画 | 10ms | 587 | 49.00ms | 60.28ms | 66.47ms |

无音频并未降低原有延迟下限，因此恢复音频。最终编码器输出 PTS 年龄稳定窗口中位数约 14ms，原实现约 56–59ms；最终版本比仅实时性修复降低了编码前等待，但这轮外发数据没有证明后两项设置进一步降低外发中位数。动画 10 秒取得 587 帧，约 58.7fps；另一个 Rust measure 调用停在 pkexec 未实际启动，已结束，不能算独立复测。测试动画正常结束，最终候选继续真实投屏，电视已接受 M4/SETUP/PLAY 并持续保活。

网络仍为 TV 担任 group owner 的 2412MHz/20MHz P2P，笔记本同一无线设备的普通 LAN 为 5180MHz/80MHz。未尝试强制改变频段。临时关闭 P2P power save 的两次 100 包 ping 均无丢包：开启时平均 8.918ms、最大 41.646ms；关闭时平均 10.983ms、最大 52.074ms。没有测得改善，已恢复开启；单次顺序实验不证明关闭一定更差。未修改永久网络设置。

最终验证：29 项 Rust 测试通过，fmt、clippy -D warnings、release 构建通过；真实硬件 verify-media 输出 HEVC Main Level 4.1 1920×1080@60 与 AAC，包含周期/主动恢复关键帧。新增测试分别验证启动实时性校正保留 latency 上下界、可变帧率输入在下一帧到来前立即输出并保持 PTS、诊断样本去重及有界存储。阶段日志留在本机 .state/latency-baseline.log、latency-no-audio.log、latency-live-capture.log 和 latency-final.log。


## 逐帧配对与时间戳口径修正（2026-09-05）

对上一节的解释作修正：mpegtsmux new_packet_cb 会调整输出 buffer PTS 的起点（output_ts_offset），并让外层时间戳跟随 PCR 流；外层 TS/RTP PTS 与该包承载的视频原始 running PTS 不完全相同。此前 86.9ms/49.0ms 是外发 RTP 时间戳年龄，不足以证明采集到发包实际耗时，也不能单凭其差值声称实际减少 38ms。编码器错误选择4帧延迟的源码、查询及修复证据仍成立。

可选 Rust 诊断现在按归一化 running PTS 关联同一视频帧的采集、编码输入/输出、mux输入、视频PES首包和network.sink。PES90kHz时间戳扣除 tsmux.c 确认的3600秒偏移，配对耗时使用各阶段到达的单调时钟；最多保存1024条数字记录、每指标300样本，不保存画面。network.sink依然在UDP时钟等待之前，测量不包含无线和电视显示。解析器及配对边界测试加入后共31项测试通过。

单线程真实桌面稳定窗口（每窗口300帧）：含AAC采集→network.sink的p50为14.614/14.370/14.871ms、p95为16.371/16.189/16.872ms；无音频为14.418/14.471/14.810ms、p95为16.263/16.070/16.331ms。两组编码完成→视频PES的p50均约0.08–0.17ms。含音频采集→编码输入约12ms，编码本身约2.3ms。未观察到音频造成显著视频等待，保持现有AAC链。含音频的mux外层PTS减视频PTS约-35ms且随包变化，无音频会话约-45.578ms固定，说明不能混用两个时间戳口径。

隔离音频实验中，当前FDK→aacparse实际协商raw AAC，再转为ADTS；连续12个1024样本输入均立即得到对应输出/PES，不依赖下一输入或EOS。绕过解析器无收益。FDK的未报告算法延迟不能直接解释为多帧输出缓存。

隔离转换测试（2880×1800 BGRA/BGRx → 1920×1080 NV12，60fps）未证明单个videoconvertscale比原videoscale+videoconvert更快。原分开实现各设置n-threads=2有小幅重复收益：单线程预热后p50约6.7ms，双线程约4.4–4.7ms；4线程结果不稳定。保持相同缩放算法与等比例黑边，候选仅将两个元素各设2线程。黑边像素测试仍要求左右96像素、完整1728像素内容；真实桌面候选结果另记。

本轮原始稳定阶段日志：.state/latency-paired-audio.log、.state/latency-paired-no-audio.log。未重新获取实际外发AF_PACKET样本：本机pkexec当前要求交互认证、sudo -n也无可用授权；采用无需root的进程内配对计时，未修改系统权限。

最终双线程含音频候选已被电视接受M4/SETUP/PLAY，真实捕获仍为2880×1800 BGRA，输出HEVC Main4.1 1920×1080@60、PAR1/1和AAC。运动测试稳定窗口采集→network.sink p50为10.343/10.345/10.483ms，p95为20.307/17.904/19.485ms；采集→编码输入p50为7.690/7.815/7.982ms，编码约2.2ms。对照单线程含音频p50约14–15ms，典型耗时减少约4ms；候选p95没有改善，部分窗口高于单线程约16–17ms，不能声称所有帧更快。测量轮次内容为同类短暂动画加桌面，非严格固定像素负载；保留中位数改善、尾部波动的限制。测试窗口已自动关闭，最终投屏继续运行。

31项完整测试、fmt、clippy -D warnings和release构建通过；verify-media真实硬件输出300帧/5秒、536RTP包、299视频PES和235AAC PES、2个关键帧包含主动恢复。默认关闭所有计时探针；本次保留启用诊断的会话供观察。最终日志保存为.state/latency-paired-two-threads.log。


## HEVC 色彩描述缺失与 Rust SPS 修正（2026-09-05）

用户反馈投屏相较 HDMI 有明显饱和度差异。本机 GStreamer 1.28.6 vah265enc 的输出 caps 声称 colorimetry=bt709，但原始合成码流经 ffprobe 无法读到 color_space/transfer/primaries；FFmpeg trace_headers 明确显示 video_signal_type_present_flag=0。本机保存的上游源码 gstvah265enc.c:828 也将此字段固定为0。单独在 NV12 caps 中指定 colorimetry=bt709，码流仍然缺少这些标记，因此仅修改 caps 不足以修复。

合成2880×1800 BGRA经同一缩放与转换路径得到1920×1080 NV12。默认与显式BT.709的中心色块YUV值完全相同：红62/102/239，绿172/41/26，蓝31/239/118，白235/127/128，黑16/128/128。这验证当前转换使用BT.709有限范围，标记修正不需要人为改变饱和度。

新增纯Rust hevc_color模块，对Annex B SPS进行有界比特解析，设置video_signal_type_present_flag=1、video_format=5、video_full_range_flag=0、colour_description_present_flag=1，以及colour_primaries/transfer_characteristics/matrix_coeffs均为1。修正位置在VA编码器输出、h265parse之前，确保后续恢复关键帧复用的参数集也带正确标记。保留其他NAL、SPS剩余字段、RBSP停止位和转义规则。当前VA Main使用的SPS路径已验证；scaling-list payload、PCM、SPS短期参考集、HRD和SPS扩展等未支持结构会明确拒绝，避免错误改写。未添加FFmpeg运行时依赖，也没有修改系统GStreamer。

独立验证：合成8帧、19个NAL中，仅索引2的SPS改变；其他NAL逐字节相同。修正后ffprobe返回yuv420p、color_range=tv、color_space=bt709、color_transfer=bt709、color_primaries=bt709。原始及修正后均解码8帧、24883200字节，SHA256同为0a25f14219cad13cc030e427ead0b7ae1c780d9f3925eda182b2b1d4d7f4408f。该一致性指解码的YUV像素，不代表电视的RGB呈现必定与HDMI一致。

37项测试通过，包括真实SPS、幂等、缺失VUI/描述、错误字段、截断/转义、非SPS保持及GStreamer探针保留PTS/DTS/duration/offset/flags且不等待下一帧。fmt、clippy -D warnings、release构建通过。第一次verify-media因现有投屏占用RTCP端口失败；停止原会话后重试通过：300帧、536RTP包/5秒，299视频PES、235AAC PES，包含主动恢复关键帧。

电视已接受修正版HEVC1080p60和AAC，实际桌面捕获仍为2880×1800 BGRA、PAR1/1，保持比例与左右黑边。未改变电视图像参数。尝试只读查询pictureMode等设置时电视返回不允许这些键，未据此猜测当前图像模式。饱和度差异是否消除仍以用户对照为准。会话日志保存在.state/hevc-color-fix.log。


## MPEG-TS PCR 播放预留试验（2026-09-05）

用户暂时停止色彩排查，回到端到端延迟。本机GStreamer1.28.6 tsmux.c定义TSMUX_PCR_OFFSET=TSMUX_CLOCK_FREQ/8，ts_to_pcr将PTS/DTS减去125ms生成PCR；默认bitrate=0路径使用此关系。这是接收端播放时钟的预留，与此前采集→UDP入口约10ms的进程内耗时不同。[源码](https://raw.githubusercontent.com/GStreamer/gstreamer/1.28.6/subprojects/gst-plugins-bad/gst/mpegtsmux/tsmux/tsmux.c)。

合成真实TS检查：40个同包视频PES/PCR的PTS−PCR严格等于125ms；将所有PCR提前75ms后严格为50ms。视频相对最近PCR为125–158.322ms，修改后50–83.322ms；音频为125–180.322ms，修改后50–105.322ms。改变元素创建顺序可让PCR PID从视频4113变为音频4352，因此修正不能只针对视频PID。

新增纯Rust transport_clock模块，在每批已就绪TS合并时调整所有PCR，正确处理33位base和27MHz extension回绕；保留PES、RTP外层时间戳、OPCR、保留位、音视频内容。先校验整批再写，错误不造成部分修改；125ms默认路径直接保持原字节。公开参数--pcr-lead-ms允许25–125ms，默认125ms，本轮显式试验50ms。OPCR是原始参考时钟，保持不变；当前两类合成流均未出现OPCR。

验证：42项测试通过，fmt/clippy -D warnings及release构建通过。合成TS调整前后的全部变更字节仅位于PCR，解码后音频和视频各自SHA256保持一致。正式Rust verify-media分别运行50ms和125ms，每次5秒300帧、536RTP包、299视频PES、235AAC PES、2个关键帧含主动恢复；各118个PES/PCR配对分别严格符合50ms/125ms。测试证明码流时钟关系正确，不证明LG一定减少75ms实际显示延迟。

真实会话已使用--pcr-lead-ms 50进入PLAY（HEVC Main4.1 1080p60+AAC，P2P接口50），日志确认PCR提前75ms，持续收到RTSP200保活。当前保持该试验会话，默认CLI仍为125ms。尚无相机显示时差测量或用户效果确认，因此未把协议时间戳变化等同于实测E2E减少。日志副本.state/pcr-lead-50-live.log。


### 25ms 与 0ms 边界试验（2026-09-05）

用户表示50ms可能更快但难以精确分辨，并明确愿意继续推向极限。CLI及媒体设置范围从25–125ms放开为0–125ms，默认125ms保持；25ms将PCR提前100ms，0ms提前125ms。42项测试、fmt、clippy -D warnings、release构建通过。正式verify-media分别验证25ms及0ms：每次118个同包PES/PCR配对均严格符合目标，300帧/5秒、536RTP包、299视频PES、235AAC PES、2个关键帧包含主动恢复。

25ms真实会话已进入PLAY，输出HEVC Main4.1 1080p60及AAC，比例和色彩设置保持。是否实际降低显示时差，以及用户能否感觉差异，仍不能由协议保活或这些发送端验证替代。

25ms会话保持约90秒后主动停止（5429编码帧），未报告发送端媒体错误。0ms首次重连停在RTSP协商前，尚未提交媒体参数；重试后电视接受HEVC/AAC并请求PLAY，日志确认PCR提前125ms、目标预留0ms，继续保活。当前保留0ms试验会话，默认125ms未改。尚无用户稳定性确认或真实显示时差测量，不声称端到端为0或已进一步下降。25ms/0ms日志分别为.state/pcr-lead-25-live.log、.state/pcr-lead-0-live.log。
