# Shift+拖选复制(WezTerm 式鼠标兜底)设计

日期:2026-11-09
状态:已批准(用户确认「批准,开工」),实施中

## 意图

应用启用鼠标上报时(Claude Code 启动即如此),当前 tay 把所有指针事件
编码进 PTY,本地拖选让位(kitty 语义,25cd076)。用户要求:按住 Shift
拖选时像 WezTerm 一样由终端自己选中并在松开时复制,事件不进应用。

## 方案(已批准)

1. Shift+左键按下:`mouse_report()` 顶部短路返回 false —— 事件不发给
   应用,落回本地选区;应用侧完全无感(收不到按下,无捕获状态)。
2. 按住拖动:抑制 1002/1003 移动上报,本地选区扩展。
3. 松开:有非空选区 → `copy_selection()`(arboard,已有);零拖动点击
   只清空不复制。
4. 普通拖选(未按 Shift)行为不变,不自动复制。
5. 非目标:Shift+滚轮滚动历史、拖动中途按 Shift 夺回、选区同步应用。

## 实现

- 新增 `crates/tayd/src/shift_select.rs` 纯状态机:
  - `bypass_report(mods, button) -> bool`(shift && 左键)
  - `ShiftSelect { active }`:`begin(shift)` / `active()` / `end(has_selection) -> bool(是否复制)` / `cancel()`
  - 路由决策与复制决策全部可单测(无 Ctx 依赖,沿用 mouse.rs 模式)
- app.rs:字段 `shift_select`;mouse_report 按下/移动旁路;本地按下置
  active、松开 `end()` 为真时 `copy_selection()`;取消路径(hide/toggle/
  close/zoom 等 5 处 dragging 清零点)同步 cancel。

## 验证

- 单测:状态机全转移;旁路决策(shift+左键 true,其余 false)。
- 回归:全 workspace 测试、clippy/fmt、三套烟测。
- 真机验收:用户在 Claude Code 里 Shift 拖选 → 松开后 Ctrl+V 粘贴;
  隔离实例冒烟无崩溃。打包 pkgrel 52 → 53。
