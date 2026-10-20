# 合成器实测矩阵

> 2026-10-20。事实基线:**仅 KDE Plasma 6 Wayland 有实测记录**;
> wlroots 系(sway / niri / river / labwc)、GNOME、Hyprland **全部待人工验证**。
> 本文是可执行清单,不是测试报告。未执行的项目保持 `- [ ]`,
> 不得凭代码审阅、单元测试或别的合成器结果勾选。
> 已有结论见 docs/architecture.md §8 与 docs/usability-review.md
> 「尚未实机验收」。

## 0. 结论边界

- 已有实测(仅 KDE Plasma 6 Wayland):
  - `tools/usability-smoke.py` 在隔离的真实 KDE Wayland 桌面通过,
    覆盖冷启动、标签生命周期、设置、热重载、Lua 预算拒绝与截图;
    后续 Lua 配置轮次又连续两轮通过真实 Wayland 回归。
  - `tools/fish-smoke.py`、`tools/protocol-smoke.py`、
    `tools/scrollbar-smoke.py` 有 KDE 下的运行记录;fish 截图已目视
    检查,滚动条脚本以钩子分发与日志断言判定,协议脚本验证真实
    PTY 应答顺序。详见 docs/usability-review.md。
  - 这些结果**不能**外推到其它合成器。
- 未实测:本矩阵其余全部项目。尤其物理键盘、F12/portal 系统授权与冲突、
  真实中文输入法、多显示器拔插、锁屏恢复,在 KDE 上也没有完成
  端到端人工验收。
- 本机限制:单显示器,无法实测真实显示器热插拔。`TAY_REBIND=1`
  只强制走一次 surface 重建路径,不等于拔线。
- 勾选条件:实际执行了「如何验证」中的步骤,记录合成器版本、日期、
  二进制版本、日志与截图;失败项保持未勾选并写下现象。
  `cargo test` 通过不等于本节通过。

## 1. 通用准备

在目标合成器的 Wayland 会话中构建并运行同一份二进制:

```sh
cargo build --workspace
```

脚本均使用隔离的 `XDG_RUNTIME_DIR` / 配置目录,不会替换或终止
已有用户实例;失败时打印日志与截图路径。

| 脚本 | 覆盖 | 不覆盖 |
|---|---|---|
| `python3 tools/usability-smoke.py` | 冷启动、`toggle/show`、标签、设置、最大化/还原、Lua 热重载与预算拒绝、截图 | 物理键盘、portal 授权、真实 IME、多屏、锁屏 |
| `python3 tools/fish-smoke.py` | fish 启动及 CJK / Nerd Font / 组合字符渲染,输出截图 | 输入法输入;可用 `--starship-config` 引用本地 Starship 配置 |
| `python3 tools/protocol-smoke.py` | 真实 PTY 的 CSI 6n / OSC 11 / CSI 14t 应答 | 只验证协议应答 |
| `python3 tools/scrollbar-smoke.py` | 指针分发入口的滑块拖动、跨栏释放、轨道点击 | 通过 `TAY_SCROLLBAR` 钩子分发,不是物理鼠标注入 |

可用的启动钩子(`crates/tayd/src/main.rs` 模块文档):

| 变量 | 用途 |
|---|---|
| `TAY_START_VISIBLE=1` | 启动即显示,覆盖 `start_hidden` |
| `TAY_SHOT=路径[,帧数]` | 渲染 N 帧后保存 PNG 并退出 |
| `TAY_REBIND=1` | 第 15 帧强制 surface 重建(仅验证回退路径) |
| `TAY_IME=文本` | 第 11 帧注入 IME Commit(仅验证内部提交链路) |
| `TAY_EXEC=命令` | 启动后在 shell 中执行命令 |
| `TAY_SCROLLBAR=1` | 运行滚动条指针分发自测 |

每个合成器建议按同一格式记录,避免把不同环境的结果混在一起:

```text
合成器 / 版本:
会话:Wayland(或 XWayland 下运行)
GPU / 驱动:
二进制:git rev 或包版本
日期:
结果:逐项通过/失败 + $XDG_RUNTIME_DIR/tayd.log + 截图路径
```

## 2. wlroots 系(sway / niri / river / labwc)

> niri 实际基于 Smithay 而非 wlroots,但与其余三者一样使用
> wlr-layer-shell;本节目录按使用场景合并,但**每个合成器必须
> 分别完成全部 8 项**,不能互相替代。

### 2.1 合成器侧配置与输出查询

| 合成器 | 全局键绑定与重载 | 输出查询 | 备注 |
|---|---|---|---|
| sway | `~/.config/sway/config` 加 `bindsym F12 exec tay toggle`;`swaymsg reload` | `swaymsg -t get_outputs` | |
| niri | `~/.config/niri/config.kdl`;`tay config` 会打印它认为的 niri 片段,按本机 niri 语法核对 | `niri msg outputs` | 保存配置后通常自动重载,以本机版本为准 |
| river | 启动脚本中用 `riverctl map` 把 F12 绑定到 `spawn 'tay toggle'` | `wayland-info`;若合成器提供 output-management 再用 `wlr-randr` | 修饰键写法与命令以本机 `riverctl` 手册为准 |
| labwc | `~/.config/labwc/rc.xml` 中 `<keybind key="F12"><action name="Execute" command="tay toggle" /></keybind>` | `wlr-randr` | 重载可用 `labwc --reconfigure`,以本机版本为准 |

注意:wlroots 系通常**没有** GlobalShortcuts portal 后端。
`tay doctor` 显示 portal 不可用是预期结果;此时用上表把 F12 绑到
`tay toggle`,而不是等待系统授权弹窗。

### 2.2 检查清单(四个合成器各做一遍)

- [ ] **呼出 / 收起(含失焦自动隐藏)**
  - 如何验证:先运行 `python3 tools/usability-smoke.py`,确认脚本
    无 panic 且截图非空;再启动 `tayd`(或让 `tay toggle` 自动拉起),
    依次执行 `tay show`、`tay hide`、`tay toggle`;用 2.1 的合成器
    绑定按 F12。点击其他窗口测试失焦隐藏,执行 `tay pin` 后重复一次。
  - 通过:窗口从顶部滑入并占满所选输出宽度;再次触发能收起;
    `tay pin` 期间失焦不隐藏;日志出现 `热呼出 <N>ms`,
    无 `panicked at`。KDE 上的 16ms/1ms 不作为其它合成器的通过阈值,
    只记录本机数值与实际卡顿感。

- [ ] **layer-shell 行为**
  - 如何验证:`wayland-info | grep zwlr_layer_shell_v1` 确认协议存在;
    以 `WAYLAND_DEBUG=1 tayd 2>wayland-debug.log` 启动,检查是否有
    `zwlr_layer_surface_v1` 创建/commit;在背后放一个最大化普通窗口,
    反复呼出/收起。
  - 通过:协议存在且实际创建了 layer surface;窗口位于普通窗口之上,
    占满顶部宽度;背后窗口的几何不因呼出/收起而重排。
    当前实现使用 `Layer::Top`、`TOP|LEFT|RIGHT` 锚定、
    `exclusive_zone=0`(`crates/tay-window/src/lib.rs`);
    若观察到桌面为窗口预留空间或触发平铺重排,记失败。

- [ ] **GPU 渲染**
  - 如何验证:运行 `python3 tools/fish-smoke.py`(检查 CJK/Nerd Font
    与颜色);再用
    `TAY_START_VISIBLE=1 TAY_SHOT=/tmp/tay-gpu.png,180 tayd` 抓一帧。
    可选:`vulkaninfo --summary` 记录驱动;需要看后端时加
    `RUST_LOG=info,wgpu_core=info,wgpu_hal=info`。
  - 通过:截图非空且文字清晰、颜色正确、无空白或花屏;日志有
    `截图已保存`,无 wgpu/Vulkan 报错、无 panic。仅 OpenGL 回退
    能出图也算渲染通过,但要记录实际后端;不要把 Vulkan 当作硬性前提。

- [ ] **IME**
  - 如何验证:`wayland-info | grep zwp_text_input_manager_v3` 确认协议;
    启动 fcitx5 或 ibus,聚焦 tay 后切换到中文,输入「中文」并观察
    预编辑与候选框;提交后确认 shell 收到文本。可追加
    `TAY_IME=中文` 验证内部提交链路。
  - 通过:预编辑覆盖层出现在光标处,不挤动后续文字;候选框位置
    跟随光标;提交只出现一次,无丢字/重复;日志有 `IME 提交 N 字节`。
    `TAY_IME` 绕过合成器输入法,只能证明 Commit→PTY→渲染链路,
    **不能代替**真实输入法验收。

- [ ] **portal 全局热键 F12 授权**
  - 如何验证:先运行 `tay doctor`,记录 `热键 portal` 一行与
    `XDG_CURRENT_DESKTOP`;若存在 GlobalShortcuts 后端,重启 daemon
    后按 F12,完成/拒绝系统授权弹窗并观察日志
    `portal 热键已由系统确认` 或 `全局热键不可用(portal 失败)`。
    若不存在后端,使用 2.1 的合成器绑定;再在合成器里把 F12 另绑
    给其它命令,确认按该合成器规则只有一个动作生效,并记录结果。
  - 通过(wlroots 的预期):`tay doctor` 明确报告 portal 不可用并打印
    合成器绑定片段;绑定后的 F12 能呼出/收起;冲突时行为可解释、
    不挂起。**不要**把 portal 授权当作 wlroots 的必需项。

- [ ] **多显示器**
  - 如何验证:用 2.1 的输出查询命令记录输出名;把配置中的
    `output` 设为其中一个输出名,执行 `tay quit` 后重新启动
    (`tay toggle`),确认窗口落在该输出;再清空 `output` 记录默认落点。
    有条件时拔掉/接回外接显示器,并在可见状态下重复。
    单显示器机器可运行 `TAY_REBIND=1` 检查 surface 重建日志,
    但必须标注「仅模拟,未拔线」。
  - 通过:指定输出时窗口全宽落在该输出;默认输出选择可复现;
    拔线后不崩溃、PTY 会话不中断、重新接回后能再次呼出;
    日志无重复 surface 或 panic。当前实现的选择顺序是
    「配置名 → 首个有信息的输出 → 合成器默认」,并不读取指针位置;
    验收时按实际行为记录,不要按 README 的「每输出独立呼出」
    直接判定通过。

- [ ] **锁屏恢复**
  - 如何验证:用该合成器支持的会话锁(如 swaylock,其余以本机
    环境为准),分别在窗口可见和隐藏时锁屏;解锁后执行
    `tay toggle`,检查窗口位置、尺寸与终端会话;同时观察锁屏期间
    按 F12 是否被投递。
  - 通过:解锁后窗口仍在正确输出与高度,内容与 PTY 会话连续;
    没有旧 surface 残留、重复窗口或 panic。锁屏期间热键未投递
    不直接判失败,照实记录该合成器的行为。

- [ ] **clipboard / data-control**
  - 如何验证:`wayland-info | grep zwlr_data_control_manager_v1`;
    执行 `printf 'tay-clip-中文' | wl-copy`,在 tay 中按
    `Ctrl+Shift+V`;在 tay 中选中文本按 `Ctrl+Shift+C`,再在终端执行
    `wl-paste`;另开一个编辑器做双向跨应用粘贴。
  - 通过:两个方向都能传递 UTF-8/中文,内容完整;无挂起或崩溃。
    若合成器没有 data-control,记录实际降级行为(例如只能单向),
    不要勾选为通过。

## 3. GNOME(Mutter)

> 当前实现只创建 wlr-layer-shell surface;Mutter 不提供该协议。
> DESIGN §6 记录的方向是「普通 toplevel + xdg-activation + 官方
> GNOME 扩展」,尚未实现。因此本节多数项目当前预期是阻塞的。
> 先验证失败模式是否可诊断,待 toplevel 路径落地后再逐项验收。

- [ ] **启动与失败模式(前置)**
  - 如何验证:`wayland-info | grep zwlr_layer_shell_v1` 应无输出;
    以 `RUST_LOG=info tayd` 启动,记录退出码与日志。
  - 通过(当前阶段):日志明确出现
    `事件循环退出: 合成器不支持 wlr-layer-shell` 一类错误,
    没有 panic、没有留下孤儿进程或占用 socket。此项只证明
    失败可诊断,不代表 GNOME 可用。

- [ ] **呼出 / 收起**
  - 如何验证:当前直接执行 `tay toggle`、`tay show`、`tay hide`,
    记录是否出现窗口或仅后台进程。toplevel 降级落地后,再按
    wlroots 的步骤测试动画、失焦隐藏与 `tay pin`。
  - 通过(当前预期):无窗口,且日志/CLI 返回可解释;不得勾选。
    实现 toplevel 后:窗口出现、可收起、失焦隐藏与 pin 正常。

- [ ] **layer-shell 行为**
  - 如何验证:`wayland-info | grep -E 'zwlr_layer_shell_v1|xdg_activation_v1'`;
    观察 GNOME 是否会为普通 toplevel 提供置顶/定位能力。
  - 通过(当前预期):确认 wlr-layer-shell 不存在;记录 GNOME 版本
    与 portal 版本。当前没有替代路径,因此该项保持未勾选。

- [ ] **GPU 渲染**
  - 如何验证:当前无 surface,无法呈现;可先记录
    `vulkaninfo --summary` 与 `glxinfo -B`(如已安装)作为环境信息。
    toplevel 落地后运行 `tools/fish-smoke.py` 与 `TAY_SHOT`。
  - 通过:截图文字清晰、颜色正确、无 wgpu 报错。当前阶段预期
    无法执行,保持未勾选。

- [ ] **IME**
  - 如何验证:`wayland-info | grep zwp_text_input_manager_v3` 确认协议;
    当前无窗口无法聚焦,只能记录协议可用性。toplevel 落地后
    用 fcitx5/ibus 实测预编辑、候选框与提交。
  - 通过:真实输入法能输入中文且提交一次;`TAY_IME` 仅作补充,
    不能代替。当前保持未勾选。

- [ ] **portal 全局热键 F12 授权**
  - 如何验证:运行 `tay doctor`;GNOME 48+ 的 GlobalShortcuts
    后端按 DESIGN §6 记载可用但有已知 `ConfigureShortcuts` 问题,
    本仓库未实测。若 portal 存在,启动 daemon 后按 F12 并完成
    授权弹窗;同时测试与系统其它 F12 绑定的冲突。
  - 通过:授权结果与日志一致,按 F12 能 toggle,冲突可解释。
    在无窗口的当前实现下,最多只能确认 portal 存在;
    不能据此宣称 GNOME 热键可用。

- [ ] **多显示器**
  - 如何验证:记录 GNOME 输出布局;当前无窗口,无法验证落点。
    toplevel 落地后按 wlroots 的步骤测试 `output` 配置、
    默认落点、拔插与缩放。
  - 通过:窗口落在预期输出、拔插后会话存活、无重复窗口。
    当前保持未勾选。

- [ ] **锁屏恢复**
  - 如何验证:使用 GNOME 自带锁屏;当前 daemon 无法创建窗口,
    只能记录启动失败与 socket 清理。toplevel 落地后再测试
    锁屏/解锁后的呼出与 PTY 连续性。
  - 通过:解锁后窗口正常、会话连续、无 panic。当前保持未勾选。

- [ ] **clipboard / data-control**
  - 如何验证:当前 daemon 在创建窗口层时即退出,无法做窗口内
    复制粘贴;可记录 `wayland-info | grep data_control` 的协议存在性。
    toplevel 落地后再做 `wl-copy` / `wl-paste` 双向测试。
  - 通过:双向 UTF-8 粘贴正常。当前保持未勾选。

## 4. Hyprland

> Hyprland 提供 wlr-layer-shell;GlobalShortcuts 依赖
> `xdg-desktop-portal-hyprland`(DESIGN §6 记载 0.52.1+ 原生协议,
> 本仓库未实测)。以下每项都需实际执行,不能按设计文档勾选。

### 4.1 环境与绑定

```sh
hyprctl version
hyprctl monitors
hyprctl layers
```

`~/.config/hypr/hyprland.conf` 加:

```text
bind = , F12, exec, tay toggle
```

重载:`hyprctl reload`。若 portal 后端可用,也可保留 portal 绑定,
但两者不要同时对 F12 生效,以免重复触发。

### 4.2 检查清单

- [ ] **呼出 / 收起(含失焦自动隐藏)**
  - 如何验证:`tayd` 启动后执行 `tay show` / `tay hide` / `tay toggle`;
    按 F12;点击其他窗口测试失焦隐藏;`tay pin` 后重复。
  - 通过:窗口从顶部滑入并占满输出宽度;再次触发收起;
    `tay pin` 期间不失焦隐藏;日志有 `热呼出 <N>ms`,无 panic。

- [ ] **layer-shell 行为**
  - 如何验证:`wayland-info | grep zwlr_layer_shell_v1`;
    `hyprctl layers` 应能看到 namespace 为 `tay` 的 layer surface;
    在平铺窗口前反复呼出/收起。
  - 通过:layer surface 实际存在并位于普通窗口之上;
    背后窗口不重排;呼出/收起不改变工作区布局。

- [ ] **GPU 渲染**
  - 如何验证:`python3 tools/fish-smoke.py`;再用
    `TAY_START_VISIBLE=1 TAY_SHOT=/tmp/tay-gpu.png,180 tayd`。
    记录 `hyprctl monitors` 中的缩放;多缩放输出时观察文字清晰度。
  - 通过:截图非空、CJK/Nerd Font 清晰、颜色正确;无 wgpu 报错
    或 panic。记录实际后端(Vulkan/OpenGL)。

- [ ] **IME**
  - 如何验证:`wayland-info | grep zwp_text_input_manager_v3`;
    启动 fcitx5(确保 Wayland 前端)或 ibus;聚焦 tay 输入中文,
    观察预编辑层与候选框;提交后确认 shell 收到。
    可追加 `TAY_IME=中文` 验证内部链路。
  - 通过:预编辑不挤动文字、候选框跟随光标、提交仅一次;
    无丢字/重复。`TAY_IME` 不能代替真实输入法。

- [ ] **portal 全局热键 F12 授权**
  - 如何验证:确认已安装 `xdg-desktop-portal` 与
    `xdg-desktop-portal-hyprland`;运行 `tay doctor`;启动 daemon
    后按 F12,完成或拒绝授权弹窗,检查日志
    `portal 热键已由系统确认` / `全局热键不可用(portal 失败)`。
    再在 Hyprland 配置里把 F12 另绑给其它命令,确认只有预期动作
    生效并记录结果。
  - 通过:portal 授权结果与日志一致;F12 能 toggle;冲突时行为
    可解释、不挂起。若 portal 不可用,合成器绑定必须能独立工作;
    此时不要把 portal 项勾选为通过。

- [ ] **多显示器**
  - 如何验证:`hyprctl monitors` 记录输出名与缩放;配置 `output`
    指向其中一个并重启 daemon;清空 `output` 记录默认落点;
    有条件时拔插外接显示器。单显示器可用 `TAY_REBIND=1` 观察
    surface 重建,但标注为模拟。
  - 通过:指定输出时窗口全宽落在该输出;默认落点可复现;
    拔插后不崩溃、PTY 连续、可再次呼出;无重复 surface。
    当前输出选择顺序为「配置名 → 首个有信息的输出 → 合成器默认」,
    不读取指针位置,按实际记录。

- [ ] **锁屏恢复**
  - 如何验证:使用 hyprlock(或本机实际锁屏);分别在窗口可见/
    隐藏时锁屏,解锁后 `tay toggle`;观察锁屏期间 F12 是否投递。
  - 通过:解锁后窗口位置/尺寸正确、PTY 会话连续、无旧 surface
    残留或 panic。锁屏期间热键未投递照实记录。

- [ ] **clipboard / data-control**
  - 如何验证:`wayland-info | grep zwlr_data_control_manager_v1`;
    `printf 'tay-clip-中文' | wl-copy` 后在 tay 中 `Ctrl+Shift+V`;
    在 tay 中选中并 `Ctrl+Shift+C`,另开终端 `wl-paste`;
    再做一次跨应用双向粘贴。
  - 通过:双向 UTF-8/中文内容完整,无挂起或崩溃;缺少
    data-control 时记录降级行为,不勾选为通过。

## 5. KDE Plasma 6(基线,仅供对照)

KDE 是当前唯一有实测记录的合成器,但已有记录不等于全部人工验收完成。

- 已有记录:`tools/usability-smoke.py` 在真实 KDE Wayland 通过,
  后续 Lua 配置轮次连续两轮真实 Wayland 回归通过;
  `tools/fish-smoke.py`、`tools/protocol-smoke.py`、
  `tools/scrollbar-smoke.py` 有运行记录;fish 截图已目视检查,
  滚动条脚本以钩子分发与日志断言判定,协议脚本验证真实 PTY 应答。
  详见 docs/usability-review.md。
- 尚未人工验收:物理键盘事件、桌面 F12 授权/冲突、真实中文输入法
  切换、跨应用剪贴板、多显示器拔插、锁屏恢复。
- 本矩阵的 KDE 行可复用上述记录,但新增项目仍按第 1 节的格式
  单独记录,不把旧结论自动带入新版本。

## 6. 汇总表(填写用)

当前全部空格表示「未验证」;只有实际执行并留档后才能改成通过/失败。

| 合成器 | 版本 | 日期 | 呼出 | layer-shell | GPU | IME | portal F12 | 多屏 | 锁屏 | clipboard |
|---|---|---|---|---|---|---|---|---|---|---|
| KDE Plasma 6 |  |  | 部分记录 | 部分记录 | 部分记录 | 未人工 | 未人工 | 未实测 | 未实测 | 未人工 |
| sway |  |  |  |  |  |  |  |  |  |  |
| niri |  |  |  |  |  |  |  |  |  |  |
| river |  |  |  |  |  |  |  |  |  |  |
| labwc |  |  |  |  |  |  |  |  |  |  |
| GNOME |  |  | 阻塞 | 阻塞 | 阻塞 | 阻塞 | 部分可查 | 阻塞 | 阻塞 | 阻塞 |
| Hyprland |  |  |  |  |  |  |  |  |  |  |

「部分记录」「部分可查」「阻塞」都必须附备注,说明是哪条命令/日志
得出的结论,不能当作通过。
