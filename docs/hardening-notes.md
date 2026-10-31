# 加固轮实施笔记(2026-10-17)

## Review 修复记录（2026-10-19）

> 计数统一:当前全量验证为 **200 个测试通过**(fmt / clippy
> -D warnings / git diff --check 全绿,2026-10-19 最后一次全量运行)。

### 后续锚点边界修复

- `SessionMeta::resolve` 同时校验历史下界、屏幕上界与尺寸合法性，
  使用 checked_sub/try_from 拒绝坐标溢出，避免非法锚点进入内核。
- 新增两项回归测试：未来行号/空屏幕/极端整数，以及复制与跳转请求拒绝。
  修复前边界测试返回 `Some(5)` 而失败，修复后全部通过。
- 未做图形桌面实测。

- 核实：统一输入预算与锚点失效世代检查已存在，本轮保留。
- 修复清屏检测依赖单次读取内字符串匹配的问题：复用 vte Parser，
  跨 flush 保留解析状态，支持分段 ED2/ED3/RIS 与等价参数 `CSI 02 J`。
  新增两项回归测试，覆盖所有双分段位置、字节原样转发、非清屏控制序列。
  修复前，ED2 在第 2 字节处分段的测试明确失败；修复后通过。
- resize 在会话命令队列中立即按 FIFO 执行，不再延迟到整批消息末尾；
  后续复制、搜索与输入看到已更新的尺寸和失效世代。
- 未进行图形桌面、锁屏或显示器热插拔实测。

两轮加固:第一轮(可靠性 → 呼出体验 → 协议/架构收敛),
第二轮(动画收敛、锚点生命周期、配置即时生效、kitty 协议对齐、
测试隔离、多显示器回退)。原则:直接改代码 + 回归测试;
已实测/已确认/未验证分开标注;不提交(按 brief 要求)。

## 第十七轮(2026-10-19 晚间):统一输入预算 + 锚点失效世代(定点修复)

范围:架构改造验收前的两个定点问题;不扩大重构;不提交。

### 1. 输入接受语义:统一预算(问题确认 → 修复)
- **确认问题**:发送端 512 KiB 在途预算(`TermSender::send` 先
  `load` 后 `fetch_add`,检查与预留非原子,并发可突破);会话
  出队**即释放**额度,随后 `enqueue_input` 用独立的 256 KiB
  预算**再次拒绝整块** —— 300 KiB 输入在 send 返回成功后仍会
  被会话层整块丢弃,两次检查不一致、释放时机错误(出队而非
  实际写入)。
- **修复(统一预算,不调大上限)**:
  - 单一共享原子计数器覆盖「跨线程消息 + PTY 待写队列」;
    `reserve_input` 用 `fetch_update` 原子完成检查 + 预留;
  - 会话出队**不释放**;`flush_pending` 返回实际写入字节数,
    `drain_pty` 按量释放(`release_input` 饱和减);会话结束
    `store(0)` 统一清零,并发迟到释放不溢出;
  - 会话端第二次预算检查**删除**(`enqueue_input` 不再拒绝,
    超限仅告警);pending 总量由统一预留约束;
  - 协议应答(`Event::PtyWrite` 经 `send_response`、kitty 应答
    直接入队)必须送达:不受上限约束但计入在途 —— 丢弃应答会
    卡住应用;
  - UI 拒绝反馈:粘贴/IME/按键 send 失败 → status toast;
  - 语义文档化:**已发送**(进通道)/ **已接受**(send true,保证
    按序完整写入或显式释放)/ **已写入**(实际写进 PTY)。
- 测试 6 个:`accepted_300k_input_is_delivered_fully`(300 KiB
  空队列完整接受 + 逐字节交付)、`wouldblock_recovers_and_delivers_
  accepted_input_in_order`、`partial_writes_release_only_written_
  bytes`、`reserve_input_is_atomic_under_concurrency`(确定性 +
  8 线程压力 + 监视线程断言)、`sender_accepts_300k_rejects_over_
  budget_and_keeps_control_free`(满预算 Shutdown 仍可发可收、
  发送失败释放、饱和减)、`response_bypasses_limit_but_is_accounted`。
- **实测(KDE)**:TAY_PASTE 钩子(第 18/19 帧注入两次 300 KiB
  粘贴);
  - **完整交付**:`TAY_SHELL` 脚本 `stty raw` + `head -c 307200
    | wc -c` → 计数文件 **307200 字节精确交付**,无拒绝日志(旧
    实现此处会命中 256 KiB 会话端拒绝);消费快时第二次粘贴也
    接受(第一次已写掉,不误拒);
  - **拒绝 + 可见反馈**:`TAY_SHELL` 慢消费脚本(`stty raw
    -echo; sleep 60`)→ 第一次接受、待写队列积压,第二次 300 KiB
    被原子拒绝(日志「输入在途预算已满(524288B),拒绝本次输入
    307200B」)+ status toast「粘贴被拒绝:终端积压,请稍后再试」
    渲染于窗口底部(截图底部 60px 带:青绿 toast 背景 6377 +
    文字像素 4423 vs 基线纯背景 115200)。

### 2. 锚点失效世代:选择/复制携带来源世代(问题确认 → 修复)
- **确认问题**:`CopyAnchors`/`ScrollToAnchor` 只带行列,不带
  来源快照的 `invalidate_epoch` —— 竞态窗口(选择→清屏→复制
  请求执行前)内,旧行号仍在合法范围却对应新文本,错位复制;
  alt-screen 进出未推进世代,主/备屏坐标可混用。
- **修复**:
  - 新类型 `AnchorEpoch`(`SessionMeta.invalidate_epoch` 改用);
  - `CopyAnchors`/`ScrollToAnchor` 携带 `epoch`;会话执行前
    `epoch_current` 校验,不匹配 → `SessionReply::Copy{stale:
    true}` 明确返回过期(不猜坐标);
  - 世代匹配但锚点被淘汰 → `stale=false, text=None`(Evicted);
  - UI:选区记录选择时刻世代(`sel_epoch`),复制/跳转携带;收到
    stale → status toast「选区已过期,请重新选择」,**不覆盖已有
    剪贴板**;Evicted → toast「内容已滚出历史」;
  - alt-screen 进出推进世代(`advance_epoch`,run 循环检测模式
    变化;主屏网格冻结,块/图像锚点保持有效,不清空);
  - 普通输出滚屏不推进世代(不以 revision 差异粗略拒绝)。
- 检测粒度:世代推进在**每批输出处理后**按模式变化判定 ——
  单批内「进 alt + 出 alt」的瞬时往返不推进世代:主屏网格在
  往返中冻结不变,往返前后旧锚点依然有效(alt4 实机验证:
  单批 `?1049h…?1049l` 不产生世代日志,属预期);跨批切换
  (vim/less 的实际行为)逐批推进。
- **世代清理的键控修复**(同日补):UI 的 epoch 变化清理原先
  连 `folded` 一起清空 —— 折叠按**稳定 BlockId** 键控(单调
  不复用),清屏/resize 后由 live-retain 按块存活裁剪,而 alt
  切换推进世代时块锚点保持有效 —— 清空会把进 vim 前用户的
  折叠全部弄丢。修复:epoch 分支只清**坐标键控**的选区与
  QuickSelect;`alt_screen_advances_epoch_and_separates_coordinates`
  增加「alt 切换不清空块」断言。
- 测试 5 个(真实 alacritty 网格):滚屏后旧世代复制原文本、
  清屏+新内容后旧选择 Stale 且新世代复制新文本、resize 尺寸
  变化才推进世代、历史淘汰 → Evicted、主/备屏幕世代隔离且
  退出 alt 后主屏锚点在新世代下仍有效。
- 顺带核查:alacritty 内核在 ED2/resize 时自行清空 vi selection
  (term/mod.rs 1750/1847),`SelectionToString` 路径无同类缺口。
- **实测(KDE)**:TAY_SEL 合成拖选复制走世代路径日志「已复制」;
  alt 进出日志「备用屏幕 进入/退出:锚点世代 N」(见下方实测)。

### 3. 实机验证(KDE Wayland,本机)
- TAY_PASTE:307200 字节精确交付 + 慢消费下第二次粘贴拒绝 + toast
  可见(见 §1);截图中 'a' 字符墙(文本像素 151822+47635+30743
  vs 基线 760);
- 持续输出 + TAY_SEL:复制成功(日志「已复制 N 字节」);
- alt-screen 进出:世代日志 + TAY_SEL 复制成功;
- 残留验证项照实列出:见文末未验证清单。

### 4. 后续定点优化(同日,非 brief 项,不扩大重构)
- **协议应答余量告警**:`MAX_RESPONSE_SLACK_BYTES = 256 KiB`,
  `response_budget_ok` 纯函数 —— 应答必须送达(不拒绝),在途
  超过「用户上限 + 余量」只告警(应用查询风暴的诊断信号);
  测试 `response_slack_boundaries`。
- **清屏检测快速路径**:`flush_text` 在文本无 ESC 且上一段以
  完整序列结尾(`clear_seq_open` 追踪)时跳过双解析 —— 高吞吐
  输出(`cat` 大文件)解析成本减半;`text_ends_in_escape_sequence`
  保守判定(误报只多跑一次检测);测试 `escape_sequence_tail_
  detection`、`split_clear_across_esc_free_tail_is_detected`
  (无 ESC 尾段跨 flush 清屏不因快速路径漏检)。
- **连续 resize 合并**:`ResizeMerger` 纯逻辑 + 排空循环同实现
  —— 连续的 configure 风暴只应用最后一个,遇其它命令先应用
  (Review 修复的 FIFO resize 语义保持);测试 `resize_merger_
  collapses_runs_and_keeps_fifo`;TAY_REBIND 实机回归干净。
- **TAY_SLOWUI 钩子**:第 15..80 帧渲染前延迟 ~30ms(慢 UI
  模拟)—— 实机:持续输出 + 慢 UI 窗口内第二次粘贴仍被正确
  拒绝(预算不因 UI 卡顿失真),无 stalled 看门狗触发(30ms <
  50ms 兜底),退出干净。

### 5. 隐藏启动呼出卡死(P1 级,实机复现 → 修复)
- **触发条件**:start_hidden 启动 → 合成器对完全移出屏幕的窗口
  不回帧回调(设计行为)→ 2s 后遮挡 watchdog 判定 stalled →
  此后呼出(show/toggle)的 RedrawRequested 被 stalled 丢弃 →
  **窗口永远呼不出**(用户实测复现:无 UI、日志仅「合成器 2s
  无帧回调,暂停渲染」)。此前未暴露:toggle 测试都在 2s 窗口内
  呼出,帧回调由 50ms 兜底重挂,stalled 从未触发。
- **修复(两层)**:
  - 完全隐藏(margin + 高度 ≤ 0)是设计状态,排除在遮挡判定外
    (`occlusion_stall` 纯函数 + 测试 `hidden_window_never_
    classifies_as_occluded`;WindowState 新增 `top_margin` 跟踪);
  - 呼出是用户显式意图:`Ctx::wake_present()` 清除 stalled,
    show/toggle 路径调用 —— 可见时真实遮挡(锁屏)后呼出也能
    立即恢复,合成器确实无响应时 watchdog 2s 后重新暂停
    (自愈不复发)。
- **实测(KDE)**:隐藏启动 3s 后 `tay show` → 热呼出 0ms、
  无 stalled 告警;toggle ×3 + show 干净(7ms/7ms)。

### 6. chrome/多标签审查轮(同日,持续 review)
- **折叠随标签保存(回归修复)**:切换标签原先 `folded.clear()`
  丢弃离开标签的折叠 —— 折叠按稳定 BlockId 键控,与 alt 切换
  同理不得因离开而丢失。修复:`folded` 移入 `TabSession`
  随切换交换(switch_tab `mem::take` / install_session 恢复)。
- **配置恢复**:审查中发现用户 config.toml 被外部写为仅
  height_percent/animation_ms 两字段(字体等丢失;核实
  save_appearance 为合并式写入、`tay config` 从不覆盖,非应用
  所为)。按日志核实值恢复完整配置(shell=/bin/bash、
  JetBrainsMono Nerd Font 12pt、start_hidden、55%/150ms)。
- **实机冒烟**:new-tab ×2 + next-tab + close-tab + toggle ×2 +
  quit 干净退出、0 错误;多标签 chrome 截图(选中标签 #2C4E5B、
  文字 #D1D6DE 渲染正确);字体/热重载链(update_font →
  relayout → Resize 会话同步)接线核实无误。
- **chrome 美化(用户反馈「有点丑」)**:颜色由终端配色
  ([colors] 配置)推导 —— 底色 = 背景提亮 7%,活动标签 =
  光标色混底色 + 加粗 + 下划线,关闭 × = 暖红,控件/设置行 =
  前景 45% 弱化;标签页样式改为 `1:shell ×`(× 为多字节字符,
  列宽改按**字符数**计,修复字节计宽在 × 后的偏移);删除
  行 0 与行 1 重复的 Hide 入口。测试 `wide_char_columns_match_
  char_count`;实机截图:底色 #1E2023、选中 #0A7D65、弱化/
  亮色文字分层正确。

### 7. 配置改 Lua(用户要求「上 lua」)
- **协议**:`~/.config/tay/config.lua` 为 WezTerm 风格脚本,
  `return` 一个配置表;字段可选、缺省用默认;类型错误/越界/
  非表返回/语法错整体拒绝,热重载沿用上次有效配置(与 TOML
  时代同语义)。
- **沙箱**:`mlua`(0.12,lua54+vendored)以 `StdLib::ALL_SAFE`
  求值 —— 无 `os`/`io`/`debug`/`package`,配置不应有副作用。
- **迁移**:旧 config.toml 首次启动自动迁移为 config.lua(值保留、
  原文件不动);`tay config` 生成 lua 模板(CLI 复用 tay-config
  的 `to_lua`,模板单一来源)。
- **保存**:`save_appearance`(A+/A- 与颜色编辑)按当前配置
  规范化重写 config.lua(手写注释丢失,已文档化)。
- 测试 5 个:模板 roundtrip、部分字段默认、错误整体拒绝、
  沙箱拦截 os/io、整数/浮点数字语义。
- **实测**:TOML → Lua 迁移值全保留(JetBrainsMono 10pt 等);
  改 animation_ms → 热重载生效;写坏类型 → 沿用旧配置并报错;
  修复 → 自动恢复;`tay doctor` 正常。

### 8. 渲染器审查轮(同日)
- **选区/光标色不生效(修复)**:渲染器硬编码选区 #395e9c 与
  光标 #00a884,config 的 colors.selection / colors.cursor 是
  死配置(用户在 Lua 里改这两项无效果)。修复:Renderer 增
  cursor_color/selection_color 字段,update_colors 热重载同步,
  rebuild_quads 改读字段。
- **鼠标拖选高亮消失(回归修复)**:重构后 build_grid 只渲染
  快照的 vi 选区,UI 鼠标选区(self.sel)**只复制不显示**。
  修复:鼠标选区优先投影(锚点 → 显示行,列钳制,同行反向拖
  选列序归一),无鼠标选区时回退 vi 选区。
- **实机验色**:临时置 cursor=#00ff00 / selection=#ff3366 →
  截图绿光标 8054px + 选区高亮 #FF3366 8671px(修复前选区
  0px);TAY_SEL 复制 21 字节正常。已还原正式配色。
- 顺带核查:文本分段合并、徽章裁剪、图像缓存按可见集保留、
  截图 BGRA 行填充处理均正确。

### 9. 键位系统审查轮(2026-10-20)
- 并行会话新增:Lua `keys` 绑定(精确修饰键掩码、动作白名单、
  重复检查)、`global_shortcut`(portal 接线)、`theme` 预设
  (default/midnight/light + 16 色盘 + `Colors::indexed` 接入
  ANSI 256 渲染)、maximize/restore(100% 高度切换 + 窗口图标
  矢量绘制)、沙箱加固(无 coroutine/pcall/load、执行预算、
  string.rep 内存预算)、整数字段拒绝 NaN/负/小数。
- **发现并修复:Lua 键位在 release 时重复触发** —— 分发位于
  `if !pressed` 守卫之前,释放事件携带相同 mods → 粘贴两次、
  maximize/settings 翻转两次净效果为零、new-tab 一次建两个。
  修复:分发移到 pressed 守卫之后(kitty release 上报仍在其前,
  不受影响)。触发条件:tay-window 对每个按键发
  press/repeat/release 三事件,release 的 mods 状态与按下时
  相同。
- 核查通过:keys 白名单/重复检查/大小写不敏感匹配、桌面
  default.lua 样例、README 文档同步、kitty_down 按 raw_code
  跟踪 release(切换标签时清空)。

### 测试与实测
- `cargo test --workspace`:**155 通过**;fmt ✓;clippy
  -D warnings 0;git diff --check ✓。

### 10. 配置复查轮(2026-10-20,用户要求「检查配置文件,再次 review」)
- **配置有效性**:用户 config.lua 已切换为 desktop.lua 样式
  (keys 全套 + Alt+1..9、global_shortcut=CTRL+grave、
  theme=midnight、opacity=0.94、padding=8、hide_on_focus_out
  =false)——`tay doctor` 全项通过;keys/theme/opacity 全部
  接线(非死配置)。
- **透明度实测**:临时置 opacity=0.6 → 截图 alpha 通道:背景
  区恰好 0.6、整体均值 0.64(文字区 1 不透明)——实现方式:
  默认背景格不画 quad、由带 alpha 的 render pass clear 提供;
  文字/控件/彩色底色保持不透明,与配置注释语义一致;截图
  捕获按 alpha mode 做预乘还原。已还原 0.94。
- **doctor 修复**:热键提示硬编码 F12,而配置已改为
  CTRL+grave —— `tay doctor` 现在读配置显示
  「✓(CTRL+grave 首次需系统确认)」。
- **小提醒(未改)**:desktop.lua 中 `{ key = "+", mods =
  "CTRL" }` 物理上不可能触发(打出 + 必带 Shift,掩码恒含
  SHIFT),属无害死绑定;opacity 0.94 效果轻微,想要明显
  透明可降至 0.85 左右。

### 11. 操作栏 Lua 化(2026-10-20,用户问「布局形状为何硬编码」)
- **设计决策(回答)**:布局与几何不放 Lua 是刻意的 —— 命中
  与渲染共享同一几何是本项目核心不变量,Lua 写布局一旦算错
  即"点不准"(连日修复的那类 bug);WezTerm/kitty 也只暴露
  颜色/宽度/位置/可见性令牌,不暴露几何;真实需求是有限枚举,
  类型化配置更安全。
- **新增 Lua 能力**:
  - `tab_bar.visible = false` 极简模式(kitty 式:栏隐藏、终端
    占满全高;设置面板展开时临时显示;快捷键不受影响)——
    `chrome::rows` 三态(0/1/3),渲染跳过 decorate,指针拦截
    与行偏移自动归零;
  - **未显式配置的 tab_bar 颜色从主题推导**(`TabBar::from_
    colors`:底色=终端背景提亮 4%、活动标签=光标色 78% 淡染、
    强调=光标色)—— 换 theme 自动协调;`TabBar::default()`
    即默认配色推导值,`Config::default()` 与
    `from_lua("return {}")` 完全一致;
  - to_lua 模板/README 同步 visible 字段与推导说明。
- **实机验证(隔离实例,不打断用户运行中的 /usr/bin 版)**:
  - 极简模式:网格 238×35 占满全高、顶部无栏、透明度 0.9
    alpha 生效;
  - midnight 推导栏:底色 #1B202C(=#171c27+4%)、活动标签
    #253A57(=光标 #58a6ff 淡染)、亮/暗文字分层正确。
- 测试:`tab_bar_derives_from_theme_unless_explicit`;全量
  **178 通过**;fmt/clippy/diff 全绿;release 已构建。

### 12. 观感再打磨(2026-10-20,用户:标签栏太浅/要时钟/要滚动条)
- **栏色加深**:推导改为「栏底比终端更暗」(blend 向黑 5%,
  VS Code/kitty 风格),活动标签 = 光标色 68% 淡染(更饱和的
  深蓝)。实测:栏 #161B25 vs 终端 #171C27,活动标签 #2B476B。
- **栏内时钟**:`chrome::Action::Clock`(非交互,窗口图标左侧
  右对齐,HH:MM,libc localtime 无第三方依赖);render 在分钟
  边界 +1s 唤醒一次重绘,空闲不额外唤醒(0% CPU 目标保持)。
- **滚动条**:kitty 风格右缘细条(3px,前景/背景中和色 40%
  透明度,非交互)—— `Grid.scrollbar: Option<Scrollbar>`(top/
  extent 网格行区间,position/size 0..1),app 由快照
  display_offset/total_lines 构造(含操作栏偏移),renderer 画
  单 quad;无滚动历史时不渲染。
- **实机验证(隔离实例,scale 2 物理 3840×1188)**:时钟数字
  像素在栏右、滚动条竖条在 x≈3808、栏色深于终端——三项
  均确认。

### 13. 并行会话新功能核查轮(2026-10-20,review)
- **外观增量保存**(tay-config 重写):UI 保存不再整体重写
  config.lua —— 原脚本包进函数(`TAY_BASE_BEGIN/END` 标记),
  UI 字段集中到文件尾(`TAY_UI_FIELDS_BEGIN`);`save_appearance_
  delta` 只合并**变化的外观字段**(未变字段保留更新过的手动
  编辑);陈旧保护:保存前重读比较 + 临时文件(create_new
  0600)+ sync + rename,期间变化即拒绝不覆盖;首次保存前
  备份 config.lua.before-ui.bak;字符串转义(\"、\\、控制字符)。
  测试 5 个(stale 拒绝/手动尾部存活/delta 保留手动主题/转义
  与备份/roundtrip)。
- **选区投影抽入 view.rs**:`selection_to_display`(鼠标锚点
  优先、行序/反向拖选列序归一、折叠感知)+ vi 选区投影,
  折叠测试 2 个 —— 此前该逻辑是 app 行内代码。
- **时钟并入保留宽度**:layout 的 `clock_width` 计入 reserved,
  标签页永不与时钟重叠(我此前版本的简化版)。
- **会话侧调色板**:新增 `SessionCommand::SetColors` 与新
  terminal.rs 模块 —— 配置颜色 → 会话 state.palette → 终端
  格颜色在会话侧解析 → 快照携带最终 RGB,单一来源;所有
  标签经 update_session_colors 同步。
- 核查结论:**未发现新 bug**;全量 **200 测试通过**;fmt/
  clippy -D warnings/git diff --check 全绿。

## 第十六轮(2026-10-19 凌晨续):收尾剩余验收项

范围:第十五轮之后残留的可执行验收项;不提交。

### 1. resize 语义入会话方法 + 交错测试
- `SessionState::resize(cols, rows) -> bool`(会话侧统一入口:
  内核重排 + 仅实际尺寸变化才失效块/图像 + 推进失效代数);
- 测试 `resize_invalidates_semantics_only_on_dim_change`:
  同尺寸(重建场景)→ 块保留、代数不动;尺寸变化 → 块清空、
  代数 +1、下一条 prompt 重建(resize 与输出交错)。

### 2. 滚轮与拖选复制实机钩子
- `TAY_SCROLL=1`(第 16 帧注入 ScrollDisplay 命令)/`TAY_SEL=1`
  (第 17 帧合成拖选 → 选择时刻定格锚点 → CopyAnchors →
  会话解析 → 应答 → 剪贴板),滚轮处理抽 `scroll_delta` 与
  WEvent 臂共用;
- **实测**:TAY_SCROLL 注入无错误;TAY_SEL 复制 21 字节
  (会话版本 18)真实终端内容;徽章 2 簇不变。

### 3. 图像预算计账表(架构 §6)
- 编码数据 / 解码像素 / 放置(数量 + 总量)/ GPU 缓存四环节的
  预算与计费方式成表入 docs/architecture.md(放置与资源一一
  对应,不重复计费)。

### 测试与实测
- `cargo test --workspace`:**109 通过**;fmt ✓;clippy -D warnings 0;
  git diff --check ✓;
- **实测(KDE Wayland)**:TAY_SCROLL + TAY_SEL(上述)、徽章
  回归 2 簇;
- **仍未实测**(环境限制,照实):真实锁屏/热插拔、合成器矩阵
  (wlroots/Hyprland/GNOME)。

## 第十五轮(2026-10-19 凌晨):架构收尾 —— 语义状态入会话 + 发送端背压

范围:收尾 brief 的核心(二/三/四)+ 状态转换测试(六)+ 文档同步
(八);不提交。核实结论:第八轮规划项除语义状态所有权外已全部
完成;本轮把最后一项做完。

### 1. 批次发布竞争:结构性消除(二)
- 旧问题:push_event 在槽位已有旧快照时只追加事件 —— UI 在
  publish 前取走会得到「旧快照 + 新事件」;
- **设计选择**:语义状态(命令块/图像/锚点)全部归**会话线程**
  所有(COW Arc),`SessionUpdate` 不再携带事件列表 —— UI 一次
  取出即得同一版本的网格与语义状态;**不存在中间态可暴露**;
  慢 UI 下旧显示快照被覆盖、语义状态在会话侧持续累积(不丢、
  不积压队列);
- 所有发布入口(PTY 输出、resize、搜索、滚动、vi、同步超时)
  走同一 `publish`(覆盖快照 + 唤醒);
- 清屏/resize 有明确批次边界:`Invalidated`/尺寸变化 → 会话内
  失效 + `invalidate_epoch + 1`,与快照同批发布。

### 2. 语义状态所有权收敛(三)
- `CommandBlockStore`、图像放置(KittyImageEntry)从 App 移入
  `SessionState`(COW Arc;`apply_event` 在会话内应用全部协议
  语义事件);UI 经快照的共享引用读取;
- **折叠状态是 UI 视图状态,按稳定 `BlockId` 持有**(不依赖
  可能变化的行号);`fold_ranges(&folded)` 在渲染时计算;
  被历史淘汰的块 ID 在应用快照时从折叠集合清理;
- 图像:资源与放置同属会话(`rgba: Arc<[u8]>` 共享,放置
  一一对应不重复计费);计数上限 + 像素总量预算 + 渲染器纹理
  缓存(可见集 retain)三层有界;
- UI 的失效视图状态(选区/QuickSelect/折叠)由快照的
  `invalidate_epoch` 驱动清理(单一机制);
- 顺手修复:surface 重建后同尺寸 Resize 曾无条件失效块
  (TAY_REBIND 复现徽章丢失)→ 仅尺寸实际变化才失效。

### 3. 发送端背压(四)
- 输入预算前移到**进入跨线程队列之前**:`TermSender` 持在途
  输入字节原子计数(512 KiB),超限显式拒绝整块(丢最新不丢
  已排队;顺序由 mpsc FIFO 保证;粘贴边界不破坏);会话出队即
  释放;Resize/Shutdown 等控制命令不受预算影响;
- 回归测试 `sender_input_budget_refuses_over_limit`:占满 →
  拒绝 → 出队归零 → 满预算下 Shutdown 仍可发可收 → 大段粘贴
  整块不截半;
- 确定性批次测试重写为 COW 模型 `session_update_accumulates_*
  `:批次 N 取出后、N+1 处理中(未发布)槽为空、旧快照不被会话
  变更污染;连续 5 批不取出 → 槽只有最新一份、语义状态全量
  保留(慢消费者有界)。

### 4. 状态转换测试(六)
- `GpuPhase` 转换抽纯函数 `on_height_request` /
  `on_resized_handshake`;测试覆盖「输出回退、高度请求、
  configure 交错」完整序列:高度请求不得覆盖重建标志(第十三
  轮卡死回归固化为受测转换)。

### 测试与实测
- `cargo test --workspace`:**108 通过**;fmt ✓;clippy -D warnings 0;
  git diff --check ✓;
- **实测(KDE Wayland)**:vi + 折叠(块 #0)+ IME 提交 + 徽章 2 簇
  同场景通过;TAY_REBIND(重建渲染器 + 重建后 2 簇);交互冒烟
  (热呼出 0ms、quit 干净);
- **未实测**(照实):真实锁屏/热插拔、滚轮注入、合成器矩阵;
  CountingHandler 与内核逐项对应关系见第七/十/十一轮笔记与
  架构 §4.4(本轮无新增影子规则)。

### 收尾核查结论(一)
1. Term 会话线程独占 ✅ 2. 选择/复制稳定锚点 ✅(第十四轮)
3. 块结构体 + BlockId + 坐标类型 ✅ 4. InputRouter/配置/映射
边界 ✅ 5. surface 状态机 + generation + 退役释放 ✅(第十三轮)
6. 快照与语义状态同版本 ✅(本轮)7. 慢 UI 有界 ✅(快照单槽 +
  发送端/会话端双预算 + 图像三层预算)8. 块/图像/锚点唯一所有者
  ✅(本轮,会话线程)。

## 第十四轮(2026-10-19 深夜):两个 P1 版本一致性修复(brief)

范围:brief 报告的两个一致性 P1;不提交。

### 1. 异步鼠标复制绑定稳定锚点(P1)
- 旧实现:选区存**显示坐标**,复制时才经当前 row_map 换算网格点,
  `CopyGridRange` 不带版本 —— 请求执行前滚屏,坐标仍合法却指向
  另一段文本,越界检查无法识别;
- 修复:**选择时刻即定格** —— `sel` 改为存历史绝对锚点
  `(HistoryAnchor, u32)`,显示单元经「与映射同源的 meta」转换
  (ViewMap 新增 `meta` 字段,`with_meta` 在 build_grid 绑定);
  复制直发 `CopyAnchors { from_line, from_col, to_line, to_col }`
  (替代 CopyRange/CopyGridRange),会话侧解析 + 淘汰裁剪;
- 回归测试 `copy_anchors_stable_across_scroll`:锚点定格后继续
  滚屏 8 行,同一锚点复制结果逐字节不变(旧网格坐标此刻已移位)。

### 2. 快照与语义事件原子发布(P1)
- 旧实现:UI 先取快照(单槽),再排空无版本号的 BlockEvent 通道 ——
  两步之间会话继续输出,旧网格配上较新的块/图像/清屏事件;
- 修复:**原子批次** —— `SessionUpdate { snapshot, revision,
  events }` 单槽一起发布/取出:快照覆盖为最新,语义事件**累积**
  到 UI 取出为止(不丢、不乱序);UI 一次取出后先应用全部事件
  再渲染,网格与块/图像/清屏必然同源;blocks mpsc 通道删除;
- `apply_block_event` 抽出(原子批次的组成部分);
- 回归测试 `session_update_accumulates_events_atomically`:
  跨两次发布的事件全部保留、顺序保持、取出后槽为空。

### 测试与实测
- `cargo test --workspace`:**106 通过**(+2 个 P1 回归);fmt ✓;
  clippy -D warnings 0;git diff --check ✓;
- **实测(KDE Wayland)**:常规注入序列(2 徽章簇)、TAY_REBIND
  (重建渲染器 + 重建后 2 簇)、交互冒烟(热呼出 0ms、quit 干净);
- **未实测**(照实):真实锁屏/热插拔、滚轮注入、合成器矩阵。

## 第十三轮(2026-10-19 夜):验收钩子抓到两个输出回退真 bug

范围:brief「有图形环境时验证…显示器变化」—— 单显示器机器无法
拔线,补 **TAY_REBIND** 验收钩子(强制走 output_destroyed 同一
路径),当场抓到两个潜伏 bug 并修复;另补 **TAY_IME** 钩子验证
中文输入链路。不提交。

### 1. 退役 surface 过早销毁 → 协议错误 + 段错误(实机复现)
- 复现:TAY_REBIND 触发重建 → `wp_linux_drm_syncobj_surface_v1:
  "Surface got destroyed already"` → wayland_backend 处理协议错误
  时读野指针 → SIGSEGV;
- WAYLAND_DEBUG 定位:sctk 0.21 的 `LayerSurface` drop 会连带销毁
  wl_surface(先角色后 surface);而 app 要等 SurfaceReplaced 事件
  才 drop wgpu 渲染器 —— **surface 死在 syncobj/buffer 销毁之前**,
  KWin 对已死 surface 上的 syncobj 销毁报协议错误;
- 修复:退役资源**延后销毁** —— `retired` 改为 (LayerSurface,
  WlSurface) 成对保活;上一代在下一次重建时才销毁(届时 app 早已
  处理完 SurfaceReplaced、渲染器已销毁)。正常退出路径无此问题
  (实测多轮 quit 干净)。

### 2. GpuPhase 被高度请求覆盖 → 重建卡死(实机复现)
- 复现:重建后 configure 到达却不重建渲染器,帧循环靠 50ms 兜底
  空转,TAY_SHOT 永不退出;
- 根因:第八轮把 `sizing`/`renderer_pending` 两个独立布尔合并为
  单一 `GpuPhase` 枚举时,`request_layer_height` 无条件写
  `AwaitingConfigure{rebuild:false}`,**覆盖了** SurfaceReplaced
  刚设置的 `rebuild:true` —— 旧实现里 renderer_pending 独立存活,
  无此问题;configure 到达后误走「只重算网格」分支,而渲染器已
  丢弃(relayout 早退)→ 永久等待;
- 修复:`request_layer_height` 只在 `GpuPhase::Ready` 时推进阶段,
  已 AwaitingConfigure 则保持原标志;顺带去掉 force_rebind 里
  重复的 OutputChanged 事件。

### 3. 验收钩子(与既有 TAY_* 同款,帧 11/15 触发)
- `TAY_IME=文本`:注入 ImeEvent::Commit(Ime 臂与注入共用
  `ime_commit` 方法)—— **中文输入链路实机验证**:提交 20 字节
  UTF-8,顶部区域文本像素 1636 → 2981(回显渲染成功);
- `TAY_REBIND=1`:强制 surface 重建 —— 输出回退路径实机验证:
  重建渲染器完成、PTY 会话保持、**重建后徽章 2 簇正确行**、
  无协议错误、正常退出。

### 测试与实测
- `cargo test --workspace`:**104 通过**;fmt ✓;clippy -D warnings 0;
  git diff --check ✓;
- **实测(KDE Wayland)**:常规注入序列回归(2 簇)、TAY_IME、
  TAY_REBIND(上述)、交互冒烟(热呼出 0ms、quit 干净);
- **仍未实测**(照实):真实锁屏/遮挡场景(watchdog 路径由
  frame_stalled 单测覆盖)、真实显示器热插拔(TAY_REBIND 走同一
  代码路径)、滚轮注入、合成器矩阵。

## 第十二轮(2026-10-19 傍晚):图像资源句柄 + 呈现门 + resize 合并

范围:对照 brief 逐条核查后收口的最后缺口;全程保持可构建、可测试;
不提交。

### 1. 图像资源句柄(架构 §6「共享资源句柄,避免逐帧复制 RGBA」)
- 核查发现的两处逐帧成本:**app 每帧 `rgba.clone()` 全量复制** +
  **渲染器每帧对全量像素 fnv1a 哈希两次**;
- 修复:`KittyImage.rgba` 与 `RenderImage.rgba` 改为
  `Arc<[u8]>`(解码时一次性 `Vec → Arc`);渲染器纹理缓存键改为
  **(图像 id, 数据指针)** —— 同一数据零哈希、零上传,指针身份
  O(1) 判定;缓存按**当前可见集 retain**,删除/替换图像的纹理
  随帧释放(替代原先「超 64 清一半」的粗放逐出);
- 分块传输中间态(payload 拼接)从句柄取出载荷,单引用路径
  不额外复制;kitty/sixel/分块相关测试断言全部迁移到 `&*arc`。

### 2. 呈现门(架构 §5.1「configure 完成后才能呈现」)
- tay-window 重绘派发显式加 `phase == SurfacePhase::Ready` 门:
  AwaitingConfigure 期间不发 RedrawRequested —— 未 configure 的
  layer surface 上带帧回调的 commit 是协议风险,且应用侧此刻
  没有渲染器,重绘必然落空;应用在 configure(Resized)后自带
  request_redraw,不丢首帧。

### 3. resize 合并(架构 §6「resize 可以合并为最新值」)
- 会话线程一轮排空中连续多个 Resize 只应用**最后一个**(PTY
  on_resize + 内核 resize + 版本 + 快照一次完成),最终状态一致,
  省去中间重排与快照发布。

### 测试与实测
- `cargo test --workspace`:**104 通过**;fmt ✓;clippy -D warnings 0;
  git diff --check ✓;
- **实测(KDE Wayland)**:注入序列徽章回归(2 簇正确行)、
  交互冒烟(热呼出 0ms、quit 干净、无错误);
- **未验证**(照实):滚轮 ScrollDisplay 无注入手段;合成器矩阵
  未实测。

### 对照 brief 的核查结论
- 一(架构文档)✅;二(会话状态/类型/失效)✅;三(命令块/视图
  投影)✅;四(窗口/GPU 生命周期)✅(本轮补呈现门);五(事件
  循环可验证性)✅;六(输入/配置/资源)✅(本轮补图像句柄、
  resize 合并;IME/粘贴路由经 InputRouter 表 + SessionCommand::
  Input 单一通道,行为由既有测试覆盖);七(验证与实施顺序)✅。
- 至此第八轮 brief 的规划项**全部实施完毕**;保留的长期事项:
  alacritty 升级时评估滚动计数接口(影子替换)。

## 第十一轮(2026-10-19 下午):失效坐标卫生 + 输入预算边界 + 应答语义收尾

范围:第十轮剩余优先级 1/2/3 收尾;全程保持可构建、可测试;不提交。

### 1. 已失效坐标上不得继续操作(架构 §7)
- 清屏/重置(Invalidated)与 resize/reflow 时,除块/图像锚点外,
  **鼠标选区与 QuickSelect 的显示坐标同属旧布局**,一并丢弃 ——
  否则会在新网格上高亮旧位置(失效坐标上继续操作);
- 复制/搜索应答的乱序/过期语义在 ProxyWake 排空处文档化:
  复制文本在会话侧解析时即定格(不可变),按到达序落剪贴板 =
  last-wins;坐标失效由会话侧 resolve/越界校验把关(返回 None
  即不复制);revision 保留作审计,搜索应答不做 UI 动作。

### 2. 输入队列预算边界(架构 §6/§7)
- 入队逻辑抽为纯函数 `enqueue_input`(与 run 循环同一实现):
  预算内入队、超限拒绝整块(丢最新、不丢已排队输入、不静默截断);
- 单测 `input_queue_budget_refuses_over_limit`:4×64 KiB 恰好占满
  256 KiB 预算 → 第 5 块(1 字节)拒绝且计数/队列不变 → 出队后
  新输入可入、顺序保留。

### 3. alacritty 滚动计数接口评估(结论记录)
- 离线环境仅有 alacritty_terminal 0.26.0(锁定);公开可观测的
  内核信号:`Dimensions::total_lines`(历史上限后封顶)、
  `Grid::display_offset`(仅用户滚动改变)、光标位置(被截断);
  storage 的 rotate 重编号不可观测 —— **历史上限之后内核不暴露
  单调滚动计数**;
- 结论:CountingHandler 影子保留;上限以下由 total_lines 增量
  交叉验证(第十轮测试),上限之后影子保持单调;升级 alacritty
  时优先评估其滚动计数接口。

### 测试与实测
- `cargo test --workspace`:**104 通过**;fmt ✓;clippy -D warnings 0;
  git diff --check ✓;
- **实测(KDE Wayland)**:注入序列徽章回归(2 簇正确行)、
  交互冒烟(热呼出 0ms、quit 干净退出、无错误);
- **未验证**(照实):滚轮 ScrollDisplay 无注入手段;合成器矩阵
  未实测。

## 第十轮(2026-10-19 午):TerminalView 模块化 + 配置期望/实际 + 内核交叉验证

范围:第九轮列出的剩余优先级 1/2/3/4,全程保持可构建、可测试;不提交。

### 1. TerminalView(view.rs)—— 架构 §2.3 落地
- 网格行 ⇄ 显示行的**正向/反向映射唯一实现**抽出 app.rs:
  `ViewMap { entries: Vec<ViewEntry{start,end,marker,text}> }`,
  折叠占位行携带区间首末行(含端点),`grid_to_row` / `row_to_grid`
  / `project`(折叠感知 + 列钳制 + 视口外钳最后行);
- **顺手修复边界 bug**:原 to_display 的 `is_marker && line > gl`
  会把折叠区间**之后**的行(下一提示符行)也映射进占位行 ——
  占位行现在显式携带区间端点,区间后行映射到自己的显示行
  (单测 `fold_mapping_boundaries` 锁定);
- build_grid 改用 ViewMap(占位填充/快照单元/光标/选区/徽章/图像
  行查找全部走 `grid_to_row`/`project`);`row_map: Vec<i32>` 字段
  删除;绘制、命中、选区、QuickSelect、光标与 IME 共用同一映射;
- 单测 3 个(边界/恒等/投影钳制)。

### 2. 复制过期语义(架构 §7「锚点始终可解析,或明确失效」)
- `copy_range_on_evicted_anchor_is_none`:历史上限 10,回放 30 行后
  最老锚点复制 → None(不 panic、不返回错位内容);最新锚点正常。

### 3. 配置期望 vs 实际运行(config.rs)
- 热重载决策抽为纯函数 `reload_decision(mtime, failed_mtime, load)`
  → `Reload::{Ok(Config), Err{repeat, error}}`:无效加载保留有效
  配置,同一错误版本只告警一次,修好(新 mtime)自动恢复;错误
  详情随 Err 携带(告警不丢信息);
- 逐字段生效性表进入模块文档(即时:colors/opacity/animation_ms/
  hide_on_focus_out/padding/height_percent;需重启:font/shell/
  output/start_hidden);
- 单测:成功 → Err(repeat=false) → 同版本 Err(repeat=true) → 恢复。

### 4. 内核交叉验证(架构 §4.4 评估结论落地)
- `counting_matches_kernel_history_growth_below_cap`:历史上限以下,
  内核 `total_lines` 增量必须等于影子计数的累计滚动位移 ——
  影子正确性的独立证据;上限之后内核封顶,影子保持单调
  (CountingHandler 保留,升级时评估内核滚动计数接口)。

### 测试与实测
- `cargo test --workspace`:**103 通过**;fmt ✓;clippy -D warnings 0;
  git diff --check ✓;
- **实测(KDE Wayland)**:TAY_VI + TAY_FOLD + 注入序列 → vi 进入、
  折叠块 @2、两徽章簇正确行(全部走新 ViewMap 路径);
  交互冒烟 toggle/quit 干净;
- **未验证**(照实):滚轮 ScrollDisplay 无注入手段;合成器矩阵
  (wlroots/Hyprland/GNOME)未实测。

### 尚未完成(下一步优先级)
1. 复制/搜索应答的过期丢弃策略(版本已携带,当前即用即取;
   若将来出现乱序应答再按 revision 过滤);
2. alacritty 升级时评估滚动计数接口(替换影子);
3. 输入队列预算的端到端压测(慢 UI 场景实机复现困难,单测覆盖)。

## 第九轮(2026-10-19 上午):Term 迁移到会话线程(架构阶段 3)

范围:第八轮规划中优先级 1 的迁移,全程保持可构建、可测试;不提交。

### 会话线程拥有 Term(架构 §3 终点)
- `ptyloop::run` 直接持有 `SessionState`(此前与 UI 共享
  FairMutex);UI 侧 **16 处 term.lock 全部迁移**,`self.term`
  字段删除 —— UI 不再直接接触终端内核;
- 快照单槽:`Arc<Mutex<Option<Arc<SessionSnapshot>>>>` 覆盖发布
  + 每波输出/每次状态变更后 `listener.send_event(Wakeup)`;
  慢 UI 不积压快照(取最新,旧帧丢弃);
- `SessionCommand`(10 种)替代 Msg:Input/Resize(PTY 与内核同源
  更新)/ScrollDisplay/ScrollToAnchor(会话侧解析 + 淘汰 no-op)/
  ToggleVi/ViMotion/Search(会话侧 regex + 滚动定位)/CopyRange
  (绝对锚点,会话侧裁剪到存活历史)/CopyGridRange(越界校验不
  panic)/SelectionToString/Shutdown;
- `SessionReply` 携带 `SessionRevision`:复制文本落剪贴板
  (ProxyWake 排空),搜索命中记日志;
- `SessionSnapshot`:可见区 cells + meta + 尺寸 + total_lines +
  display_offset + 光标 + vi 选区 + 颜色表 + 模式 + 光标形状;
  渲染(build_grid)从快照构建,快照缺席时渲染空白;
- 按键翻译/输入路由(vi、kitty flags、app cursor)用快照模式
  (滞后一个输出批次,与 PTY 往返同级,注释说明);
- 顺手修复:图像折叠隐藏此前把绝对行与 grid 相对区间比较
  (滚动后折叠隐藏失效,改经 resolve 后的网格行)。

### 测试与实测
- 新增 `session_snapshot_and_copy_range`(真实网格回放:5 行窗口
  10 行输出 → cum 7、快照只含可见区、按块复制 line1..line8、
  越界网格点 None、跳锚点生效);
- `cargo test --workspace`:**97 通过**;fmt ✓;clippy -D warnings 0;
  git diff --check ✓;
- **实测(KDE Wayland)**:注入序列回归(两徽章簇正确行)、
  TAY_VI(命令路径进 vi)+ TAY_FOLD(折叠块 @2)、交互冒烟
  toggle/quit 干净、热呼出 0ms、无错误;
- **未验证**(照实):滚轮 ScrollDisplay 命令路径无注入手段,由
  单测与代码审查覆盖;wlroots/Hyprland/GNOME 等合成器矩阵仍
  未实测。

### 尚未完成(下一步优先级,顺延)
1. TerminalView 模块化(row_map/折叠/选区投影抽独立模块);
2. 复制/搜索应答的过期丢弃策略(当前复制文本即用即取,版本
  已携带,后续按需);
3. 配置期望 vs 实际运行配置的显式结构;
4. alacritty 滚动计数接口评估(替换 CountingHandler 影子)。

## 第八轮(2026-10-19 凌晨):架构设计优先的重构与加固

范围:docs/architecture.md(单一事实来源)+ 分阶段落地,全程保持
可构建、可测试;不提交。终态与「尚未完成的迁移边界」见文末。

### 阶段 1:架构文档
- docs/architecture.md:核实过的线程拓扑、六职责边界、状态唯一
  所有者、跨线程消息表、坐标模型(HistoryAnchor/GridPoint/
  DisplayPoint/SessionRevision/SurfaceGeneration)、失效规则、
  surface 生命周期状态机、初始化/退出顺序、分阶段迁移计划与
  每阶段的验收条件、未验证清单(不沿用旧笔记结论)。
- DESIGN §4、README 架构节同步(architecture.md 为单一事实来源)。

### 阶段 2a:会话状态原子化(架构 §3 核心不变量)
- 新增 session.rs:`SessionState { term, meta { cum_scroll, revision } }`
  合入**同一把 FairMutex** —— cum 从 app 字段移入锁内;ptyloop 每批
  输出同锁推进 term + cum + revision。**任何一次网格读取都配对
  产生它的锚点版本**,「新网格 + 旧 cum」的单帧不一致窗口被结构性
  消除(此前 app 侧应用 Scrolled 与渲染读网格之间存在竞态)。
- `BlockEvent::Scrolled` 删除(app 不再持有 cum 副本);块事件携带
  绝对行(在锁内计算),app 侧不再做任何 `± cum_scroll` 算术。
- 所有 UI 侧 Term 变更(resize/scroll_display/vi/选区)经
  `SessionState::bump()` 推进版本号;16 处 term.lock 全部改经
  SessionState。
- 顺带修复(第七轮已修,本轮并入):ptyloop 每波输出后 Wakeup。

### 阶段 2b:类型化锚点 + 转换集中
- `HistoryAnchor` / `GridPoint` / `DisplayPoint` / `SessionRevision`
  / `SurfaceGeneration` 落地;转换唯一实现 `SessionMeta::resolve`
  (锚点 → grid 行,早于历史淘汰返回 None = 显式失效);
  badge 命中/折叠区间/徽章渲染/跳转/复制全部走 resolve,
  UI 侧手写 `± cum_scroll` 清零(唯一例外:resolve 内部)。
- 内核变更检测(CountingHandler)定位写入架构 §4.4:total_lines
  增量与 cum 在历史上限以下互相印证(分块等价测试覆盖);上限
  之后内核无单调计数可观测,影子保留 —— 升级成本:alacritty
  提供滚动计数接口时换用。

### 阶段 3a:命令块结构化
- blocks.rs:元组 `(i32,i32,Option,Option)` → `CommandBlock { id:
  BlockId, prompt: HistoryAnchor, prompt_col, output_start:
  OutputBoundary, exit }`;`OutputBoundary::{Protocol, Approximate}`
  显式区分「协议提供的 C 边界」与「无 shell 集成退回 prompt+1
  的近似边界」;BlockId 稳定递增(折叠/跳转仍以锚点寻址)。
- 折叠区间/徽章/复制范围在渲染时经 resolve 校验,淘汰的折叠
  区间直接丢弃(不渲染失效锚点)。

### 阶段 4:surface 生命周期收敛
- tay-window:7 个布尔(configured/surface_configured_once/
  frame_pending/frame_armed_at/stalled + app 的 sizing/
  renderer_pending)→ `SurfacePhase::{Uninitialized, AwaitingConfigure,
  Ready, Closing}` + `SurfaceGeneration`(rebind 时 +1);
  configure/frame/closed 回调按 surface 身份校验,**旧世代事件
  不得修改新实例**;新 surface 不继承 stalled(已有);退役 surface
  每代只保留一个(已有,文档化)。
- app:`sizing`/`renderer_pending` → `GpuPhase::{Ready,
  AwaitingConfigure{rebuild}}`,三态合一(首次初始化/输出回退重建/
  热重载高度)。
- 动画 deadline/toast/帧兜底/watchdog:提取纯函数 `next_deadline`
  /`frame_stale`/`frame_stalled`(tay-window),主循环不再内联
  时间算术;受控时钟测试 3 个(最早 deadline、50ms 重试 vs 2s
  watchdog 分档、阈值边界)。

### 阶段 5:InputRouter + 资源策略
- 新增 input.rs:按键/IME/粘贴路由纯决策 `KeyRoute::{QuickSelectConsume,
  Local(动作), ViConsume, KittyProtocol, Term}`,优先级(QuickSelect
  → 第一档折叠/vi 开关 → vi → kitty → 第二档快捷键 → 终端)集中
  在 `classify`;快捷键表单一来源 `is_local`,app 键盘臂改经
  classify/is_local,kitty 分支提取为 `handle_kitty_key`;4 个单测
  锁定优先级(vi 内折叠不被吞、kitty 应用内复制粘贴仍拦截等)。
- PTY 写队列预算:256 KiB,超限**显式拒绝 + 告警**(不静默截断);
  纯函数 `accept_input` + 单测;flush 出队同步减计数。
- 图像总量预算:解码 RGBA 总量 64 MiB 上限,超限逐出最旧放置
  (纯函数 `kitty::eviction_count` + 单测;单图上限 64 MiB 在解码侧,
  像素数据每图一份不逐帧复制)。

### 阶段 7:架构不变量测试
- **`chunk_split_equivalence_full_term`**:同一字节流(wrap、宽字符、
  DECAWM 开关、局部滚动区、CSI 999S/2T 截断、alt-screen 进出)
  在**每一个切分点**重放,网格内容/累计滚动/模式/光标/历史
  全量等价 —— 用真实 alacritty 网格与回放数据,不依赖手工行号。
- 慢 UI 无界队列:输入预算单测 + 既有写队列按序交付测试;
  图像预算单测。
- 备用屏幕不污染主屏坐标 / 历史淘汰失效:既有回归保持通过。

### 测试与实测
- `cargo test --workspace`:**96 通过**(上轮 85;新增 session 2、
  窗口层 3、input 4、kitty 预算 1、分块等价 1;blocks 测试全部
  迁移到类型化 API);fmt ✓;clippy -D warnings 0;git diff --check ✓。
- **实测(KDE Wayland,本机)**:注入序列回归(局部区 1S + DECAWM
  长行 + 两枚徽章 → 两徽章均在正确行,与第七轮一致);
  TAY_VI/TAY_FOLD harness 无 panic;交互冒烟 toggle/quit 干净、
  热呼出 0ms。
- **未验证**(照实):wlroots/Hyprland/GNOME、锁屏恢复、显示器
  热插拔、IME 其它合成器 —— 未实测;surface 世代校验由协议
  语义 + 单测覆盖,不沿用旧笔记结论。

### 尚未完成的迁移边界(下一步优先级)
1. **Term 迁移到会话线程**(架构 §3 终点):UI 经 SessionCommand
   操作、渲染读 SessionSnapshot(可见区 + 锚点 + revision,不复制
   全历史)。当前 Term 仍为 UI/session 共享锁 —— 同锁已保证
   网格与锚点同版本,但 UI 仍有 16 处直接 lock(本轮已全部改经
   SessionState,迁移时逐个换成命令)。
2. 复制/搜索应答携带 SessionRevision、过期操作拒绝重试;
3. TerminalView 模块化(row_map/折叠投影/选区投影从 app 抽出);
4. 配置期望 vs 实际运行配置的显式结构(当前无效热重载保留旧值
   已实现,逐字段即时性表在 docs)。
5. 内核补丁评估:alacritty 若提供滚动计数接口,替换 CountingHandler
   影子(升级成本已记录,架构 §4.4)。

## 第七轮(2026-10-19 深夜):两个滚动计数 P1

### 1. DECAWM 关闭时 wrap 不再计数(P1,用户报告)
- 触发条件:CSI ?7l 后 `Term::wrapline` 直接返回(不换行、不滚动、
  input_needs_wrap 保持),计数影子的 `will_wrap` 没查 LINE_WRAP →
  底行长文本产生幻影计数(用户复现:计数 +5,实际 0)。
- 修复:will_wrap 增加 `TermMode::LINE_WRAP` 守卫;挂起影子语义
  不变(关闭期间照样置位,与 Term 的 needs_wrap 一致 —— 重新开启
  DECAWM 后 pending wrap 恢复计数)。
- 回归测试:`counting_handler_ignores_wrap_when_line_wrap_off`
  (40 字符 0 计数 + 历史不增长 + 重开 DECAWM 后 pending 触底 +1)。

### 2. 显式滚动按实际行数计数(P1,用户报告)
- 旧实现 `scroll_up(n)` 直接 +n,两个错:局部滚动区内的 CSI S
  只动区域内内容(历史不动),不该计;请求超过区域高度时内核截断
  (Term::scroll_up_relative 内 `min(n, 区域高)`)—— 5 行窗口
  CSI 999S 实际滚 5 行,旧代码计 999。
- 修复:CountingHandler 增加 `scroll_region` 影子跨 chunk 存活
  (CSI r 的 1-based top;bottom → 0-based [start,end) 外加设置时
  行数;top>=bottom 的无效值忽略与 Term 一致;RIS 归位;Term 在
  resize 时把区域重置为全屏 —— 按「设置时行数 ≠ 当前行数」识别
  并归位):
  - `scroll_up(n)` / `delete_lines(n)`:仅全屏区域(0..screen)且
    (M 还需)光标在第 0 行 → `+min(n, screen)`;
  - `scroll_down(n)` / `insert_blank_lines(n)`:仅全屏区域(且 L
    光标在第 0 行)→ `-n`;`n >= screen` 时整个区域被清空
    (grid.scroll_down 的 reset 分支,无平移)→ 0;
  - `linefeed`/`newline`/input wrap:光标在滚动区域底行**且区域
    全屏**才计(与 Term::linefeed 的 `next == scroll_region.end`
    对齐);区域外底行的 linefeed 不滚动,也不计。
  - 局部区域操作一律计 0 —— 不能作为整个历史坐标系的平移(brief)。
- 已知限制:顶部锚定的局部区域(0..k<rows)在 alacritty 内部走
  「整体 rotate + 恢复固定行」,历史确实会增长;区域内内容上移、
  区域外不动,单标量 cum 无法精确表达。按 brief 规则计 0,
  仅全屏区域计。
- 回归测试 5 个:局部区 CSI 1S、CSI 999S 截断、CSI 2T/999T、
  区域底行 linefeed、CSI M/L(全屏顶/局部)。

### 3. 顺带发现:块事件交付延迟(P1 级,实测发现)
- ptyloop 处理完输出不唤醒 app(唯一 Wakeup 在空闲超时分支),
  块事件最多延迟 500ms 才被 drain;只写一次 wake 管道的场景
  (TAY_EXEC)下,后续突发的 133 标记会一直躺在通道里 —— 实测
  第二波 A/C/D 不送达、徽章缺失。
- 修复:每波 PTY 输出处理后 `listener.send_event(Event::Wakeup)`
  (与 alacritty 事件循环同款)。实测:两波注入序列全部即时送达。

### 实测(KDE Wayland,本机)
- **注**:本机 fish 4.9.3 已不再内置发送 OSC 133(逐字节抓包确认),
  实测改为 bash + 注入序列(与 docs/shell-integration.md 的验证
  方式一致);shell-integration 文档的「fish 4 零配置」条目已不成立,
  见下方文档修订。
- 注入 A/C + OUT1 + D;7 → 局部区 CSI 1S + ?7l 下 40 字符 + ?7h
  → A/C + OUT2 + D;3:两枚红色徽章均渲染在正确行(修复前 OUT1
  徽章因幻影 +40 漂移到网格 -40 而消失)。
- CSI 5S:第二块 PromptStart 绝对行 cum=5;CSI 999S:cum=14
  (窗口实际行数,截断正确)。
- 全量:85 测试通过、clippy 0 警告、fmt 通过。

## 第六轮(2026-10-19 晚间):第五轮验收前的三个修复

### 1. wrap 计数修复(P1,用户报告)
- 第五轮的 input 重写在后续清理编辑中**被误回退**(只剩 pending
  判定、无挂起置位、有调试残留),wrap 触底漏计(测试 1 vs 4)。
- 重新落地宽度精确版:挂起置位 `printed_at + width >= cols`、
  宽字符占位 wrap(col == cols-1)、wrap 判定与 Term::input 语义
  逐字符对齐;测试恢复为完整三 feed 主体,cum == 4 精确断言。

### 2. alt-screen 守卫(P1,用户报告)
- CountingHandler 的四个计数点(input wrap / linefeed / newline /
  scroll_up / scroll_down)全部检查 `TermMode::ALT_SCREEN`:
  备用屏幕的滚动不移动主屏内容,不计入 cum_scroll —— 否则
  vim/less 退出后主屏命令块与图像锚点整体偏移。
- 回归测试:进 alt → 10 行滚动 + SU → 退出,cum 保持 0;退出后
  主屏滚动正常计数(+2)。

### 3. stalled 重置(P1,用户报告)
- 输出回退重建 surface 时清除 stalled(遮挡 watchdog 是旧 surface
  的状态):否则新 surface 无首帧呈现,恢复路径无法启动。
- rebind_layer 同步重置 configured / frame_pending /
  frame_armed_at / stalled。

## 第五轮(2026-10-19):三个 P1/P2 修复

### 1. 历史缓冲满后锚点(P1,用户报告)
- 上一轮"grid 行 + 当前 history"在 history 封顶后不再单调(history
  不增长、grid 行持续减)→ 锚点重复编号(5 行窗口、3 行历史复现:
  第 7~11 行锚点全部变成 7)。
- **修复:累计滚动位移**。vte 0.15 的 `Handler` 接口是精确信号源
  (linefeed/input/scroll_up 逐事件可见),实现 `CountingHandler`
  代理:转发全部 66 个 Term 实现的方法,精确计数推入历史的滚动
  (linefeed 触底 +1、input 的自动换行触底 +1 —— 用 unicode-width
  与 term 的 pending-wrap 语义逐字符对齐、跨 chunk 的
  wrap_pending 影子、scroll_up(n) ±n)。
- 锚点 = grid 行 + cum_scroll(只增不减);ptyloop 经
  BlockEvent::Scrolled 逐次上报,app 累计并换算。
- **历史淘汰处理**:块按 max_span(= scrolling_history + 屏幕行)
  裁剪;复制/跳转前检查 grid 行 ≥ -history_now,越界裁剪或放弃
  (bounds_to_string 越界会 panic,已实测)。
- 回归测试 2 个:5 行窗口 + 3 行历史上限 + 10 行输出(绝对行距
  覆盖全部输出、复制裁剪到存活部分)、wrap 触底滚动精确计数
  (逐字符、pending-wrap 语义,断言 cum == 4)。
- **实测**:30 行输出滚动后,第二条命令的退出码徽章 206 个红色
  像素落在正确的 grid 行(badge abs=34 cum=22 → grid 12)。
- 限制(文档化):CSI r 自定义滚动区内的滚动不产历史,按全屏计数,
  该场景锚点可能偏移;主屏 shell 场景不受影响。

### 2. 输出移除的 layer surface 重建(P1,用户报告)
- 原实现复用已呈现过的 wl_surface 重建 layer 角色 —— 协议禁止
  对带已提交 buffer 的 surface 重新赋角色。
- **修复**:输出回退时创建**全新 wl_surface** + 新 layer 角色,
  旧 surface 退役保活(GPU 销毁 swapchain 时仍会 marshal 它);
  重置 configure/帧回调状态等待握手;`SurfaceReplaced` 事件驱动
  app:丢弃旧渲染器 → 等新 surface 的 configure(Resized)→
  重建 wgpu surface 与渲染器(create_renderer 复用首次初始化路径)
  → 重提 margin。CanCreateSurfaces 仅全局首次触发(终端不重建)。
- **未实测**:单显示器环境无法触发输出移除 —— 代码路径完整但
  明确标记未验证。

### 3. 隐藏时增大高度露出窗口(P2,用户报告)
- 高度更新只改状态机内部 margin,未提交窗口 margin(旧 -400 配
  新 500 高 → 底部露出 100px)。
- **修复**:request_layer_height 在 set_size 后同步提交
  `set_top_margin(vis.margin())`。

## 第四轮(2026-10-17 深夜):三个 P1 修复

### 1. 锚点坐标空间(P1,用户报告)
- **确认 bug**:`cursor.point.line` 是 **grid 行**(视口锚定:屏幕顶 = 0,
  scrollback 为负),输出滚动时全部行号重编号(scroll_up 的 rotate
  使内容 -1 每行) —— 我们按"绝对行"使用,滚动后复制/徽章/跳转
  全部错位。真实终端复现:5 行窗口输出 10 行,输出起点滚到 -6,
  旧代码存 1..3。
- **修复**:捕获时存**稳定绝对行 = grid 行 + history 大小**
  (`Dimensions::total_lines - screen_lines`,history 随滚动等量
  增长,绝对行不变;历史淘汰由既有 max_span 裁剪/ED 失效覆盖);
  使用处(复制/jump/徽章/折叠/图像)减 history_now 转回 grid 行。
- 回归测试:`real_terminal_scroll_keeps_anchors_valid`
  (5 行窗口 × 10 行输出:起点 -6、内容完整、scroll_to_point 定位)。
- **实测**:长输出 + 退出码 1 → 徽章像素(206 红)落在正确提示符行。

### 2. 动画调度提前停止(P1)
- `step()` 返回 None 有双重含义(未在动画 / margin 未变),调用方
  把"margin 未变"误当结束清掉 deadline → 取整平台期动画冻结。
- 修复:step 与 deadline 解耦 —— 只要 `animating()` 就重新武装
  下一动画步唤醒。

### 3. 帧回调等待覆盖动画/toast 定时器(P1)
- frame_pending 分支独占超时计算:回调延迟时等 2s、stalled 后
  无限等待,动画与 toast 反馈被吞。
- 修复:统一取帧兜底(50ms 重试/2s watchdog,仅未 stalled)、
  动画 deadline、一次性唤醒点的**最小值**;stalled 后动画/toast
  定时继续生效。

## 第三轮(2026-10-17 晚间):kitty 传输缓存 + 复制边界 + 帧回调

### 1. kitty 传输缓存与动作语义(kitty.rs/ptyloop.rs/app.rs)
- `KittyImage.delete: bool` 重构为 `action: KittyAction` 枚举
  (TransmitDisplay / TransmitOnly / Place / Delete)。
- **确认 bug**:a 键缺省路由到 `t`(仅传输)——协议缺省是 `T`
  (传输并显示);既有 4 个测试因此暴露并修复。
- **新能力**:
  - a=t 仅传输进缓存(不显示、不动光标);
  - a=p 放置缓存图像(命中回 OK 并放置,未命中回 ENOENT);
  - a=d 同步清缓存(id=0 全清);
  - 缓存 32 MiB 字节预算,超限丢最旧。
- 测试 4 个:缓存重放、a=t 不显示、ENOENT 路径、删除后 ENOENT。
- **实测**:TAY_EXEC 发 a=T + a=p,截图像素验证 760 个红色像素
  (base64 解码 → 渲染 → 放置链路全通)。

### 2. 无末尾换行输出的复制(blocks.rs/ptyloop.rs/app.rs)
- 块锚点扩展为 (prompt 行, **prompt 列**, 输出起始行, 退出码);
  `BlockEvent::PromptStart { line, col }`。
- `last_output_range` 返回 (from, end_line, end_col):提示符在新行
  行首 → 结束于上一行行尾;提示符与输出同行(无末尾换行)→
  结束于提示符前一列 —— **完整复制输出且不含提示符**(此前
  记录为限制,现已修复)。
- 真实终端测试:无末尾换行场景 bounds_to_string 验证复制
  "no-newline" 且不含提示符。

### 3. 帧回调丢失死锁(tay-window/app,重大回归修复)
- **现象**:TAY_SHOT/TAY_BENCH 连续帧模式只渲染 1 帧后停摆,
  2 秒后 stalled 把渲染彻底锁死(第二轮结束时未测此路径,回归)。
- **根因**(WAYLAND_DEBUG 定位):`pre_present_notify` 在 present
  前用**空 commit** 注册帧回调,KWin 不响应空 commit
  (`wl_callback#16.done` 永不到达);50ms 兜底 emit 时把
  `frame_armed_at` 清 None → 兜底只生效一次 → 2s 后 stalled 死锁。
- **修复**:
  1. present **之后**重新请求帧回调(`arm_frame_after_present`,
     带 buffer 的 commit 才会被合成器响应);
  2. 兜底路径改 `rearm_frame()`(强制重发)+ **重置兜底计时**
     而非清 None —— 即使回调持续丢失,循环以 50ms 兜底节奏继续,
     不再死锁。
- **实测**:TAY_BENCH 60 帧 avg 1.28ms ≈ 781fps(修复前停在第 1 帧);
  帧序列 1→6→11→16→21 正常推进。

## 第二轮(2026-10-17 下午)

### 1. 动画状态收敛(visibility.rs)
- **确认 bug**:step_at 在 margin 因取整提前到达终点后返回 None →
  vis 永远卡在 Animating(deadline 被清、P2b 渲染门失效)。
  修复:「本次无需提交 margin」≠「动画结束」——到期必收敛,
  收敛时 margin 强制为目标值。
- set_layer_h:隐藏中重锚;收起方向动画的目标同步更新;margin 不越界。
- 测试 6 个:提前取整收敛、连续相同 margin、快速反向、零时长、
  动画中改尺寸、隐藏方向收敛。
- **连带发现并修复**:animation_ms=0 时 toggle 的直达呼出分支
  没有延迟测量与首帧重绘(Margin 分支遗漏)。实测 9ms 恢复。

### 2. 命令块锚点生命周期与复制边界(blocks.rs/ptyloop.rs/app.rs)
- 锚点 = 网格绝对行号:滚动回看**不失效**;失效事件三类——
  清屏/重置(ED2/ED3/RIS 检测 → BlockEvent::Invalidated)、
  resize/reflow(列/行变化 → 清空)、历史淘汰(行距超
  scrolling_history+屏幕行数 → 裁剪最老锚点)。
- 失效即清空块/折叠/图像锚点,下一条 prompt 重建 —— 不再复制
  或跳转到错位内容。
- **确认 bug**:last_output_range 排除单行有效输出(`<= 1` 判空
  把 from==to 的单行也判掉)→ 改为 `from > to` 判空;语义文档化
  (空/单行/多行/无末尾换行/多行提示符)。
- **真实终端网格测试**:alacritty Term + ansi Processor 喂真实字节
  流,真实光标行号驱动块存储,bounds_to_string 验证复制内容
  (两行/单行/空输出/多行提示符四场景)。
- 测试:单行可复制、空输出 None、行距裁剪、invalidate、清屏序列
  检测、真实终端四场景。

### 3. 配置即时生效(app.rs/tay-render)
- 提取 relayout(width, height, force):表面尺寸变化或 padding
  热重载都走同一网格重算路径(term.resize + PTY Msg::Resize +
  IME 矩形重设);renderer 加 update_padding。
- height_percent 变化 → request_layer_height(走 sizing →
  configure → Resized;Resized 里按 sender 是否已存在区分
  「重算网格」与「重建终端」——热重载不会重建 PTY)。
- 需重启字段(font/shell/output/start_hidden):reload 时逐一 warn
  并注明「运行中保持 <实际值>」——期望配置与实际运行配置显式区分。
- **实测**:height 45→60 → 「呼出高度重设为 648px」+ 网格重算;
  padding 3→8 → 网格列数变化;shell 变化 warn(运行中保持 /bin/bash);
  删除配置文件 → 保持上次有效配置(重启回默认,已文档化)。
- 首次创建/原子保存/连续编辑/删除场景均有实测或明确语义。

### 4. kitty graphics 协议对齐(kitty.rs/ptyloop.rs/app.rs)
依据官方规范 https://sw.kovidgoyal.net/kitty/graphics-protocol/ 核实后修复:
- **f=24/32 载荷是 base64 编码**(此前按 raw 解析 —— 自制样本与
  解析器共享同一错误);全部测试样本改为 base64 协议形式;
- **C 键语义修正**:协议默认 = 光标移动(右移 cols + 下移 rows,
  新增 CursorAfter::Move),C=1 = 不移动(此前实现反了);
- m 分块:整串 base64 后切块(非末块 4 的倍数)——重组方向已对,
  解码与协议一致;
- **查询/放置应答**:a=q 回 `OK`(声明支持),a=p 回 `ENOENT`
  (无传输缓存,诚实拒绝);响应经 PTY 写队列回客户端;q=2 静默键
  未实现(始终应答,已记录);
- a=d 不带定位键 = 删除全部可见图像(id=0 语义);
- 解码安全:checked arithmetic + 4096² 像素上限 + 64 MiB 字节上限;
- **未支持(明确记录)**:传输缓存(a=t 与 a=T 都按显示处理)、
  帧动画、删除 scope(d=...)、I=number 语义、q=2 静默。
- 测试 6 个:query/place 解析、光标默认、delete-all、标准样本、
  查询应答字节、分块样本(base64)。

### 5. 测试隔离与严格检查(ipc.rs)
- 锁/socket 路径全部可注入(acquire_lock_at/bind_at),测试用
  **每测试独立临时子目录**,不再碰真实 XDG_RUNTIME_DIR / 运行中
  socket;并发加锁、活跃 socket 不可删、陈旧 socket 重建、全新绑定。
- 清理弃用 API(icy_sixel::sixel_encode_default → SixelImage::from_rgba)
  与 clippy 严格模式问题(荒谬比较、多余借用)。
- **严格检查全过**:`cargo fmt --all --check` ✓、
  `cargo test --workspace --offline` 70 通过、
  `cargo clippy --workspace --all-targets --offline -- -D warnings` 0 错误。

### 6. 多显示器生命周期(tay-window/app)
- output_logical_size 优先 layer 绑定输出(此前取第一个输出,
  多显示器高度可能算错);
- **output_destroyed 明确回退**:绑定输出消失 → 重建 layer 角色
  到合成器默认输出(复用同一 wl_surface,wgpu 零重建)+ 通知应用
  重设高度与 margin(Event::OutputChanged);
- update_output(尺寸/缩放变化)→ 同通知,应用幂等重算。
- **未实测**:输出热插拔/移除需第二台显示器,本机单显示器 ——
  代码路径存在但未在真实多显示器环境验证,明确标记。

## 第一轮(2026-10-17 上午)

## 阶段 1:可靠性

### P1a 单实例 / IPC(ipc.rs 重写)
- flock 锁文件为权威互斥;bind 前先 connect-probe,**绝不删除活跃
  实例的 socket**;陈旧 socket(daemon 已死)才清理重建。
- 限制:单行 ≤ 4096 字节、并发处理线程 ≤ 16、读超时 5s。
- 新增 `pin` 命令;响应语义文档化:`ok` = 命令已入队(非完成)。
- **实测**(KDE Wayland):第二个 daemon 静默退出 0;`kill -9` 后
  重新启动自动恢复;并发 `tay toggle` 无竞态。
- 测试:`lock_is_exclusive`。

### P1b PTY 写队列(ptyloop.rs)
- **确认的真实 bug**:alacritty 的 PTY master 是 O_NONBLOCK
  (tty/unix.rs `set_nonblocking`),缓冲满时 `write_all` 返回
  WouldBlock → **输入静默丢失**。
- 修复:待发队列 + 偏移冲刷;writable 兴趣只在有积压时监听
  (Level 模式防热自旋;注意 polling 的 `modify()` 默认 Oneshot,
  必须 `modify_with_mode(..., Level)`);WouldBlock 保留队列,
  Interrupted 重试。
- 连带修复:`rx.try_recv().is_err()` 探测会**吞掉刚到的消息** ——
  消息处理移出 WAKE_KEY 分支,统一在事件后 drain;UI 端断开即退出。
- 回归测试:`write_queue_retains_and_flushes_when_blocked`
  (非阻塞管道填满 → 队列保留 → 释放后按序完整交付)。

### P1c 流扫描器(ptyloop.rs StreamScanner)
- chunk 边界孤立 ESC 破坏配对(`"…\x1b" + "]133;A…"`):新增
  esc_pending 跨 chunk 配对(Text/Osc 两态)。
- OSC ST(ESC \)跨 chunk 读取失败:修复。
- 终止符回放:转发必须保留原始终止符(BEL/ST),不再硬编码 BEL。
- 超长序列防呆:原样转发前缀而非无提示丢弃(终端仍能完整消费)。
- 回归测试:`chunk_boundaries_do_not_change_events`(9 类输入 ×
  全切分点归一化等价)+ `osc_forward_keeps_st_terminator`。

### P1d 配置(tay-config)
- **确认的真实 bug**:`parse_hex` 按字节切 UTF-8 串,`"#aéé"`
  (恰 6 字节)会在字符边界处 **panic** → 先验 ASCII 十六进制再切分。
- 热重载失败(语法/越界)保留上次有效配置(app.rs try_load 路径);
  同一错误版本只告警一次,修好自动恢复。启动路径仍回退默认值。
- 范围校验:`height_percent` 1–100、`opacity` 0–1、`animation_ms`
  0–5000、`padding` 0–100、`font.size` 1–200;`nan`/`inf` 一律拒绝。
- 文档:README + `tay config` 模板标注即时生效 vs 需重启字段。
- 回归测试:`parse_hex_multibyte_is_rejected_not_panicked`、
  `range_validation_rejects_bad_values`。

## 阶段 2:呼出体验

### P2a 事件循环阻塞化(tay-window)
- wake 通道从 mpsc 换成 **UnixStream 管道**,进 `poll()`:空闲时
  无限阻塞(-1)替代 5ms 轮询。
- 动画 deadline:`set_anim_deadline`(步进 ~8ms 粒度,结束即清);
  一次性唤醒点 `request_wake_at`(toast 到期用)。
- 帧回调未归时按 50ms 兜底/2s watchdog 点限时唤醒,stalled 后无限
  阻塞(回调到达本身是 Wayland 事件)。
- **实测**:空闲(含隐藏)0.0% CPU(此前 5ms 轮询持续占用);
  热呼出 1–16ms 无退化。
- **连带发现并修复的 M1 遗留 bug**:text-input-v3 的 Done 处理器
  对每次 done 都回 commit,与 KWin 形成 commit→done ping-pong 风暴
  (WAYLAND_DEBUG 定位,~kHz 往返、40% CPU)。修复:只在状态真正
  变化(dirty)时 commit —— 协议正确语义,Done 是回执不是请求。

### P2b 隐藏时跳过 GPU 渲染(app.rs)
- 完全收起时 PTY 输出不请求重绘;render() 早退(截图/基准模式除外)。
- 内容在下次呼出时按当前网格重绘,不丢任何东西。

### P2c 动画
- 反向动画从**当前实际 margin** 出发(新 `margin` 跟踪字段,
  P3c 后并入 VisibilityController),不再从动画起点跳变;
- `animation_ms = 0` 直接到位,不走 1ms 伪动画。
- 测试:`reversal_starts_from_current_position`(注入时钟)。

### P2d pin 动作
- `tay pin` 切换失焦自动隐藏的暂停;CLI 端接线补上(本轮发现
  daemon 端有、CLI 端漏了)。

### P2e 剪贴板 + QuickSelect
- **持久剪贴板**:TayApp 常驻一个 arboard 实例,全部读写复用 ——
  Wayland/X11 剪贴板数据归属存活连接,每用新建即弃会丢数据
  (arboard 自己的警告即此意)。**实测**:OSC 52 → 持久实例 →
  `wl-paste`/Klipper 均读到。
- QuickSelect 打开动作分离:普通标签只复制,Shift+标签同时
  xdg-open(防误触即开网页);toast 反馈(底部 1.2s)+ 日志。
- 宽字符列映射:regex 偏移是**字节**序号,渲染器 overlay 是**字符**
  序号 —— 两级换算后标签位置正确(CJK 前不再画偏)。
- 回归测试:`wide_char_shifts_match_column`。

## 阶段 3:协议 / 架构收敛

### P3a kitty graphics(kitty.rs + ptyloop.rs)
- **m=1 分块传输**:块载荷不按块解码(base64 等编码可跨块切分),
  首块控制键 + 原始载荷拼接,m=0 末块重组完整序列统一解析;
- 内存预算:累计 32 MiB,超限丢弃并告警;流中断(无末块)永不产出
  半幅图像;孤立 m=0 块丢弃;
- 查询(a=q)不回应答 —— 已文档化,应用应靠标准能力探测;
- 回归测试:3 块标准样本拼接、缺末块丢弃、预算超限丢弃、无 m 键
  普通传输会作废在途分块、协议标准样本。

### P3b 命令块(ptyloop.rs + app.rs + blocks.rs)
- alt-screen 守卫:ALT_SCREEN 模式内的 A/C/D 全部抑制;
- C 边界跟踪:块元组扩展为 (prompt, 输出起始 C, 退出码),
  按块复制只复制**纯输出**(无 C 退回旧行为);
- bash/zsh 集成片段 + reflow 限制文档:docs/shell-integration.md,
  DESIGN §8.4 同步;
- **实测**(截图像素验证):`133;C` + 退出码 1 → 行尾红色 `✗ 1`
  徽章正确渲染(x≈1874 即右端 badge 区)。

### P3c 拆分
- `visibility.rs`:VisibilityController(Vis 状态机、margin、pin、
  注入时钟的 step_at 便于确定性测试);
- `blocks.rs`:CommandBlockStore(块锚点、折叠、徽章、导航、
  复制范围);
- app.rs 委托,状态不再散落;50 测试(clippy 0 警告、fmt 通过)。

## 验证汇总
- `cargo test --workspace`:**50 通过**(加固前 30;新增 20 个,
  其中回归测试 15:写队列 1、chunk 等价 2、parse_hex 2、范围校验 1、
  宽字符 1、kitty 分块 3、标准样本 1、可见性 4、块存储 5 的多数)。
- `cargo clippy --workspace`:0 警告(加固前 42)。
- `cargo fmt --check`:通过(加固前未过)。
- 实测(KDE Wayland,本机):空闲 CPU 0.0%、热呼出 1–16ms、
  反向动画、pin、剪贴板外部读取、徽章渲染、IPC 并发 —— 全部通过。
- **未验证**:wlroots 系(sway/niri/river/labwc)与 Hyprland 的
  behavior 未实测 —— 本机为 KDE。IME ping-pong 修复基于协议语义
  (smithay 客户端示例同款行为),在 KWin 实测正常,其它合成器的
  text-input 行为未验证。

## 输出移除不再退出 + wgpu/wl_surface 生命周期(2026-10-26)

### 问题确认(实机日志 + coredump)
- 坞站/USB-C 闪断 → 所有 Wayland 输出同时消失(plasmashell 等全部
  KDE 应用打印 `There are no outputs - creating placeholder screen`,
  用户感知为「状态栏崩掉」);KWin 按 wlr-layer-shell 协议给绑定在
  该输出上的 layer surface 发 `closed`。
- 旧行为:`closed` → `CloseRequested` → `request_close(Window)` →
  `shutdown` → 整个 daemon 退出(2026-10-26 10:54:48 实机复现,
  服务干净退出)。
- 叠加两个段错误(9 个 coredump):
  - 10-16 六次:退出 drop 链 `TayApp → Renderer → wgpu
    Surface::unconfigure → vkDestroySwapchainKHR` 在已销毁的
    wl_surface proxy 上 marshal(libwayland-client 读 0x174)。
    根因:`SurfaceReplaced` 带 `sender.is_some()` 守卫,无会话时
    忽略 → 渲染器未 drop → 下一次重建 `retired.clear()` 释放了
    wgpu 仍在引用的 proxy。
  - 10-19 三次:主循环 `EventQueue::flush` 在 wayland-backend
    `store_and_return_error` 里段错误,KWin 报 `error in client
    communication`(连接级失败路径;本轮未单独修复,需持续观察)。

### 修复
- `closed` 不再退出:与 `output_destroyed` 共用 `output_gone()` ——
  还有其它输出 → `rebind_layer(None)` 回退;全部输出消失 → 退役
  layer 角色、`phase = Unmapped`、`awaiting_output` 挂起,等
  `new_output` 恢复重建。合成器整体退出由连接断开/分发错误路径
  兜底(事件循环 Err 退出)。`Event::CloseRequested` 变体删除
  (用户主动退出走托盘/IPC/动作,与合成器事件无关)。
- `SurfaceReplaced` 无条件 drop 渲染器(去掉会话守卫);
  `Resized` 握手以新增 `initialized` 字段区分「首次 init」与
  「输出回退重建」,无会话的回退重建不会误开新会话。
- `finish_shutdown` 显式 `renderer = None`:wgpu 先于窗口层销毁,
  不再依赖 drop 顺序。
- 自查修复(同轮 review):挂起分支释放 KDE 模糊对象并清帧在途
  状态;`set_background_blur` 挂起期间只记录意图不建对象;
  `OutputChanged` 守卫改为 `initialized`(无窗格时输出恢复也要
  重设高度,否则挂起期间热重载过高度会握手失败卡死)。

### 验证
- `cargo test --workspace` 全绿(183+28+14)、`cargo check` 通过。
- 本机实测:启动 → `tay status`(IPC/GPU 就绪)→ `tay hide` →
  `tay quit` 干净退出,无 coredump(10-16 型 teardown 段错误已消除)。
- **未验证**:真实输出移除/恢复路径(需热插拔显示器)未实测 ——
  逻辑与现有 TAY_REBIND 验收路径同构,留待下次坞站闪断实测。

## 输出移除修复的二轮 review 定点修复(2026-10-26)

- **retired 销毁时机**:`rebind_layer` 里的 `retired.clear()` 移到
  `drain_pending` 末尾。原实现假设「rebind 时上一次 SurfaceReplaced
  已送达」,但 wake_present 的 rebind 发生在 ProxyWake 直接派发阶段,
  其 SurfaceReplaced 要等下一轮 drain —— 期间输出销毁会再次重建并
  clear,销毁 wgpu 仍持有的 surface(10-16 coredump 同款 UAF)。
  现在统一在 drain 末尾销毁:此刻此前入队的 SurfaceReplaced 都已
  送达,app 已 drop 渲染器。
- **最后一台显示器拔掉的判断**:sctk 0.21.1 的 `output_destroyed`
  在把 wl_output 移出 OutputState **之前**回调,`outputs()` 此刻仍
  列出正在销毁的输出,导致 `output_gone` 误判「还有其它输出」而走
  回退重建。现在 `output_gone(dying)` 显式排除正在销毁的输出;
  closed 路径传 None(极端时序下至多多走一轮 bounded 重绑后收敛)。

## 全库 review 的 13 项既有问题定点修复(2026-10-26)

review 代理对 diff 之外做了全库扫掠,13 项确认后全部修复:

- **快捷键**:`ActivationGate` 恢复 2s 陈旧按下自愈(一次丢失的
  Deactivated 不再永久锁死全局 toggle;测试反转为
  `stale_press_self_heals`)。
- **关闭保护**:`close_guard` 除前台进程组外,扫描 shell 的存活
  子进程(R/S/T;Z/X 除外)—— Ctrl+Z 挂起任务与后台任务关闭时
  不再静默被杀;`live_children` DFS /proc children。
- **热重载/撤销**:重载结果与运行配置相同(自己的保存写盘触发
  mtime 监视)时不再清 `appearance_undo`/`settings_drag`,只有
  外部编辑覆盖才清。
- **IPC 切设置页**:`Settings`/`SettingsPage` 与 chrome 路径一致,
  清 editing/settings_drag/settings_feedback(否则方向键被编辑态
  吞掉、陈旧 drag 在松手时误写撤销)。
- **选图器**:`try_recv` 区分 Empty/Disconnected—— portal 线程
  退出后清 `wallpaper_picker`,按钮不再永久失效。
- **退出持久化**:最终会话写盘失败也 `mark_clean`(干净退出不再
  被下次启动误判为崩溃恢复;**三轮 review 回退此项**:失败保留
  .running 走崩溃恢复,避免静默回滚到更旧布局)。
- **配置钉写**:`save_appearance_source` 去掉 tab_bar 整表钉写条件,
  只钉写相对基线真正变化的字段;`from_lua` 改为**先按主题推导
  tab_bar 再覆盖显式字段**(部分指定的表不再落回默认值 ——
  整表钉写原是为了扛住这个 roundtrip,现在由推导补齐,不再需要);
  主题切换保留用户定制字段(≠ 当前主题推导的 tab_bar 颜色跨主题
  存活,如手写时钟色)。
- **壁纸**:滑条每步换 key 时不再清 `wallpaper`/`img_cache`
  (陈旧图作回退不闪屏,kitty 内联图不重传);worker 记住最近一次
  解码失败的身份,同一文件版本不再每秒重试,文件变化后自动恢复。
- **操作栏**:`hit()` 按栏高映射命中行(非紧凑 3 行栏最下一行不再
  死区);帮助/编辑消息画在栏的最后一行(原 rows>=4 守卫在栏最多
  3 行时永不成立,消息从未显示)。
- **设置浮层**:快捷键页状态行/诊断提示改按 layout 的 base/step
  计算并做行存在性守卫,不再覆盖按钮或画出面板;背景页提示改指
  「图片强度」(壁纸画在底色之上,窗口不透明度调不动它)。

测试:全量 **234 通过**(新增 6 个回归:close_guard 子进程扫描、
chrome 三行命中/消息行、tab_bar 钉写、主题定制保留、快捷键自愈;
focus-smoke 未重跑)。实机冒烟:启动 → status → quit 干净退出无
coredump。

## 三轮 review:新修复交互与收敛(2026-10-26)

三轮 review 对 13 项修复本身再做一轮对抗审查,15 项发现全部
确认并修复(多为新修复之间的交互缺陷):

- **消息行不可点击**:hit() 排除消息行(否则帮助文字下方的
  隐藏/关闭列是不可见的破坏性点击目标);消息行计算改惰性
  `then(|| rows-1)`(rows=0 不再下溢)。
- **init 前重建不卡死**:SurfaceReplaced 内补 `request_layer_height`
  (OutputChanged 被 initialized 守卫忽略的窗口期,新 layer 沿用旧
  尺寸导致 ±1 握手永远失败);重建握手(rebuild=true)接受合成器
  最终高度为准,不再因钳制卡空白;未初始化时直接走 init。
- **退出持久化回退**:最终写盘失败**保留** .running(按崩溃恢复
  还原最近一次自动保存,优于静默回滚到更旧 session 文件)。
- **颜色编辑即时重推导 tab_bar**(apply 时同步,不等 mtime 重载;
  保存/重载失败也不留错配);主题切换基线改用**当前 colors 推导**
  (颜色驱动的偏移不再误判为用户定制,切主题不会留下深色栏配
  浅色标签);重推导保留布局字段与相对旧推导的定制字段。
- **tab_bar 钉写只写用户定制**:保存时与 `TabBar::from_colors`
  比较,颜色推导字段不钉(回退颜色后栏跟随主题,不被旧钉写锁死);
  布局字段按基线比较;其余表(font/colors)保持剥离基线。
- **close_guard 排除 nohup**:SigIgn bit1(SIGHUP)忽略的进程不算
  忙碌(否则确认框频繁误报,用户养成盲目 Ctrl+Enter 习惯);
  挂起(T)任务无论如何都算。
- **快捷键自愈改为解锁不触发**:2s 陈旧按下只清闸门,重发型
  合成器按住不放不再每 2s 抖动;下一次真正按下正常触发。
- **壁纸失败 30s 退避**:同一文件版本失败后不每秒重试(坏图不
  空转),退避到期或文件变化自动重试(瞬时失败可恢复)。
- **输出跟踪绑定具体输出**:output_gone 回退到仍在的**具体**
  wl_output 并记录 bound_output(None 会让后续分辨率变化/移除
  永远匹配不到);new_output 恢复时绑定到新输出;closed 把
  bound_output 作为 dying 排除(拔最后一台显示器正确挂起)。
- **设置页**:诊断提示行只在「复制诊断报告」按钮存在时画
  (h>10)。

测试:**236 全绿**(新增消息行命中、颜色编辑重推导回归等),
clippy 0 警告、fmt/diff 干净;实机冒烟启动→status→quit 干净
退出无 coredump。

## 四轮 review:子代理实测验证与收尾(2026-10-26)

四轮主代理死于 API 错误,但 10 个验证子代理完成,12 项发现
(部分带实机测量/合成器源码佐证)全部处理:

- **close_guard SigIgn 位错(实测铁证)**:掩码 bit N = 信号 N+1,
  原 `mask & 2` 检查的是 SIGINT;nohup 过滤完全失效且测试因
  reparenting 失真。改 `mask & 1`;测试改为 shell `wait` 保活 +
  直接断言,可判别。
- **busy() 短路**:先解析 stat,前台进程组占用 tty/无 tty 时不走
  /proc 子树扫描(2Hz 门控轮询里省 3 次 proc 读/次)。
- **快捷键自愈恢复触发式**:真实合成器的按住重复 ~30ms(远小于
  2s,被去重抑制),丢失 Deactivated 是真实 GNOME bug(mutter
  #4846,改修饰键中途不送 release)—— 触发式对真实场景最优,
  「解锁不触发」只会吞掉真实按下。
- **closed 不排除 bound_output**:协议只说「不再显示」,KWin 的
  Alt+F4 会无输出移除地发 closed;排除唯一输出会挂起等待一个
  永不来的 new_output。恢复为 output_gone(None)(重绑收敛),
  new_output 增加死绑定检测兜底。
- **重建分支补 vis.set_layer_h**;通用 Resized 路径在高度漂移时
  同步 vis(隐藏动画按实际高度,不留底部残影)。
- **删除不可达分支**:控制流证明 rebuild:true 被独立分支完整
  处理,混合分支里的 `else if rebuild` 与 `on_resized_handshake`
  为死代码,已删并更新测试。
- **SurfaceReplaced 去重**:初始化后不再重复 request_layer_height
  (OutputChanged 会做);显式 set_top_margin 移除(内部已提交)。
- **chrome_message_row 不构造字符串**:消息每个分支都非空,行号
  直接由 rows 推出;指针移动每事件省 ~16 次分配。
- **settings 页二行位置从 layout items 推导**:消除 base/step 重复
  与 h=5 孤儿行;壁纸 100% 提示文案改为准确描述(文字画在壁纸
  之上;真实效果是不透明度滑杆失效)。
- **壁纸基础像素缓存**:同一文件版本只解码一次,滑条每步只重算
  透明度/亮度(此前每步全量读盘+解码,4K 图几十上百 ms)。
- **首次 init ±1 钳制担忧**:子代理证伪 —— KWin/wlroots/sway/
  Hyprland 对非零请求高度不钳制,实机日志确认,仅作潜在脆弱性
  记录。

测试:**236 全绿**(含新的可判别 nohup 测试),clippy 0 警告、
fmt/diff 干净;实机冒烟启动→status→quit 干净退出无 coredump。

### 四轮冒烟自抓回归(同轮修复)
- 「高度漂移时同步 vis」的通用路径在**握手前**也会命中:初始
  configure(480)回显落入 fallthrough,把 layer_h 覆盖成 480,
  真正的 594 configure 到来时 ±1 握手永远不匹配,init 永不执行
  (daemon 持有 flock、IPC 监听已绑定但无人 accept,表现为
  `tay status` EAGAIN + 2s 帧回调 stall 告警)。
- 修复:仅 `gpu_phase == Ready` 时同步 vis/height;握手前的杂散
  configure 只走 relayout(renderer 为空时是 no-op)。冒烟复验:
  网格/首帧/status/quit 全通。

## 产品验收修复:设置打开时齿轮失效 + 矮窗口面板遮栏(2026-10-27)

用户以产品经理身份验收,报两个缺陷:

- **设置打开期间齿轮点不动**:settings_open 的指针分支无条件
  `return`,栏上(面板外)的点击永远到不了 chrome 分支 —— 齿轮
  无法关闭面板(面板内的「关闭」按钮可用,但产品预期是齿轮
  开关一致)。修复:面板矩形外的指针事件不再被消费,落到 chrome
  分支(栏命中/悬停照常);拖动中的移动与释放仍全局消费(滑条
  可拖出面板边缘)。
- **矮窗口设置面板遮挡操作栏**:`settings::rect` 居中公式在矮
  窗口时让面板顶部进入栏区,盖住齿轮/标签,且指针命中与绘制
  分离。修复:rect 增加 bar_rows 参数,面板顶部钳到栏之下,可用
  高度 = 总行数 − 栏高;退化网格钳回界内(测试 (2,1) 保持不越界)。

验证:236 测试全绿、clippy 0 警告;实机截图数值对比(设置开关
两帧)栏第 0 行差异仅时钟数字(~1.9%),面板不再触碰栏区。
完整验收矩阵(panes/lifecycle/usability/protocol/fish/scrollbar/
desktop-services/focus-product 八套烟测)见本轮验收结论。

## Review 附带修复:时钟配色死配置 + focus-smoke 截图竞态(2026-10-26)

### 1. tab_bar.clock_background/clock_foreground 从未生效
- `chrome.rs` 时钟格原来用标签栏背景 + icon_foreground,且
  `chrome_panels` 只给标签推圆角面板 —— 配置里解析/校验/序列化
  的时钟配色没有任何消费方(usability-review 验收的「独立高对比
  圆角色块」设计丢失)。
- 修复:时钟格改 `style.clock_background/clock_foreground`,
  面板循环加入 Clock(background=clock_background,四角全圆,
  无活动标记、不受 hover 影响)。
- 回归测试 `clock_uses_its_own_colors_and_rounded_panel`;
  TAY_SHOT 截图对比确认:修复前暗底浅字,修复后亮蓝圆角块+深色字
  (本机 Lua #58a6ff/#161b25)。

### 2. focus-smoke 隐藏断言竞态
- `hide()` 在动画开头释放键盘,focus-enter 立刻触发,紧接着的
  截图可能拍到滑出中的中间帧 —— 成败取决于 spectacle 启动快慢,
  animation_ms=150 时偶发误报「Tay remained visible after hiding」。
- 修复:visible/hidden 截图前各 settle `duration/1000 + 0.2s`,
  等滑入/滑出动画完成再拍(动画结束后的截图延迟不再影响结果)。
- 验证:focus-smoke 两个动画档(0/150ms)全部 PASS。

## 设计轮验收结论(2026-10-28)

覆盖提交 6fab1d7/91b9e69/f99d143/0aa40bf/227b441(消息行遮挡修复、
设计 A 栏去消息行、快捷键页清单与按页提示、极简模式恢复栏、面板
开合动画、+ 图标矢量化、toast 队列、标签芯片间隔、分隔条悬停)。

自动化矩阵(八套烟测,全部 PASS):

- panes:probe/restore/close_pane/saved_layout
- lifecycle:systemd_stop/resurrect(3 标签)
- desktop-services:20 次快捷键有序送达、托盘属性/圆角图标/菜单、
  快捷键会话重连、焦点恢复
- protocol:query/response 往返
- fish / scrollbar:截图产物正常
- usability:产品截图链路(2 标签会话保存)
- focus:animation 0/150ms 两档

单元测试 239 全绿、clippy/fmt 干净。

手动验收项(自动化不可覆盖):

1. 真实键盘触发全局快捷键 + KDE 授权弹窗(沿用模拟 Portal,不碰
   用户桌面的老待验收项)
2. 面板开合动画手感(90ms)、齿轮开关、Tab/Esc 键盘路径、按页提示
3. 页 3 快捷键清单:改 keys 后实时更新、unbound 跳过
4. 极简模式(tab_bar.visible=false)打开设置时临时恢复栏
5. toast:首次呼出 4s 提示、授权异常提示、队列接续
6. 分屏分隔条悬停高亮与拖动
7. 多标签芯片间隔、+ 图标、时钟色块

底角圆角明确不做:壁纸图像管线需单独圆角掩码,仅纯色背景可用则
两态不一致,保持直角统一。

## 输入前自动滚到底(2026-10-31)

- 问题:视图停在滚动历史里时打字,输入照发但回显与光标都在
  视口外(盲打);主流终端(VTE scroll-on-keystroke 默认开、
  Windows Terminal、iTerm2)均为「打字即跳到底」。
- 修复:`jump_to_bottom_for_input` —— 凡发往 shell 的输入
  (按键/粘贴/IME/kitty 编码键)前,快照 display_offset > 0
  且非 alt 屏幕时,先发 `ScrollDisplay(Scroll::Bottom)` 再发
  输入(同一通道 FIFO,先滚再回显)。alt 屏幕无滚动历史,跳过。
- 边界:vi 浏览、QuickSelect、折叠等本地动作走独立路径,不经过
  该钩子 —— 回看历史不受打字打断;已在底部时门控跳过,不产生
  空命令。
- 验证:新单测 `shell_input_while_scrolled_jumps_to_bottom`
  (真实 PTY:20 行输出 → 上滚 10 行 → 钩子 → 会话侧
  display_offset 归零);全工作区测试 196+29+14 全绿,clippy/
  fmt 干净。

## 滚动历史搜索(Ctrl+Shift+S,2026-10-31)

- 背景:搜索此前只在 vi 模式可用(`/` + n/N),起点是 vi 光标、
  无回绕、失败无反馈 —— 生态标准件缺失(Ghostty 1.3 也刚补)。
- 会话侧:`SessionCommand::Search` 增加 `SearchStart`(Cursor /
  Anchor+世代),regex 搜索未命中时回绕到另一端再找一次(vi 的
  wrapscan 语义);命中返回行锚点 + 状态世代,供「下一个」从其后
  继续;锚点世代/淘汰校验失败 → 明确未命中(与 CopyAnchors /
  ScrollToAnchor 同一规则,不猜坐标)。
- UI 侧:独立搜索栏(Ctrl+Shift+S)复用 vi 搜索输入与底部覆盖行;
  首次从视口顶(含顶行)向下,Enter / Shift+Enter 循环,未命中
  toast「未找到匹配」;输入期间按键全部由搜索栏消费(拦截在配置
  键位与 QuickSelect 之前,查询里的 c/f 不会触发复制/QuickSelect);
  Esc 关闭。vi 模式 `/` 语义不变(n/N 走 vi 光标)。
- 验证:新单测 `scrollback_search_anchors_wrap_and_epoch_guard`
  (真实 PTY:锚点起点、命中锚点回传、回绕再命中、世代过期、
  无匹配);全工作区测试 197+29+14 全绿,clippy/fmt 干净。

## 搜索/跳底复核修复(P0+P1,2026-10-31)

架构师 + 产品经理双重审核后落地的修复:

- **P0 未找到反馈不再走 toast**:与搜索栏同画在 `rows-1` 会互相
  覆盖 —— 改为搜索行内反馈(`搜索: {query} · 未找到`,1.2s 后
  醒来重绘清除);查询编辑/再次命中/关闭时立即清除。
- **P1a `scroll_on_input` 配置口**:默认 true(跳底);false =
  停在历史里打字保持视口不动,覆盖「边看历史边打字」场景。
  解析/get_bool 与 `disable_default_key_bindings` 同模式,
  to_lua 带出、desktop.lua 模板注释。
- **P1b 首次呼出提示补本地动作**:toast 改为「呼出/收起 |
  Ctrl+, 设置 | Ctrl+Shift+S 搜索 | Ctrl+Shift+F 速选」
  (搜索/速选是本地动作,不在快捷键页清单里,toast 是唯一
  入口;标签/分屏保留在设置页 3)。
- 验证:跳底单测扩展 `scroll_on_input=false` 不跳的断言;
  tay-config 解析/roundtrip 断言;全工作区 197+29+14 全绿,
  clippy/fmt 干净。

## 搜索体验补全(P2,2026-10-31)

架构师/产品复核 backlog 一次性落地:

- **实时搜索 + 命中高亮**:独立搜索边输边跳(alacritty 同款)——
  查询每次编辑/IME 提交后立即从视口顶重搜;命中范围由会话侧
  `search()` 写入 `SessionState.hit`(grid 行 + 列区间),随快照
  发布,UI 折叠感知投影到显示行,渲染器新增独立色块(光标色
  55% 淡染,与鼠标/vi 选区蓝分开;`tay-render::Grid.search_hit`
  + `Renderer.search_hit_color`,热重载跟随)。未命中清空高亮。
- **记住上次查询**:Esc 关闭时存入 `search_prev`(空查询不记);
  独立搜索重开预填但**不立即重搜** —— 重开本身不移动视口,
  高亮/滚动留给 Enter 或首次编辑(alacritty 同款);`search_fresh`
  标记:首次按键整体替换(打字即新查询),Enter 采纳预填后改按
  追加语义;vi 模式查询关闭后 n/N 也回退用上次查询。
- **IME 进查询框**:搜索栏打开时 IME Commit 写入查询(不发往
  shell),预编辑不在终端光标处显示;独立搜索实时重搜,vi 查询
  保持 Enter 驱动。
- **搜索栏放行窗格/标签快捷键**:配置键位命中白名单(next-pane /
  split / close-pane / zoom / new-tab / close-tab / 切标签 / hide /
  maximize)时不再被搜索栏吞掉,照常执行、搜索栏保持打开;
  切换窗格后搜索状态跟随窗格。
- 验证:搜索单测扩展快照命中范围断言(锚点/grid 行/列区间
  指向匹配词,未命中清空);全工作区 197+29+14 全绿,clippy/
  fmt 干净。
