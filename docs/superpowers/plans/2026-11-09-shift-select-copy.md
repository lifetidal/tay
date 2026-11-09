# Shift+拖选复制(WezTerm 式鼠标兜底)Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 应用开启鼠标模式时,Shift+左键拖选由终端本地选中并在松开时复制(WezTerm 语义)。

**Architecture:** 纯状态机 `shift_select.rs` 承载旁路与复制决策(无 Ctx 可单测);app.rs 在 mouse_report 入口旁路事件,本地按下/松开接线状态机,松开即 `copy_selection()`。

**Tech Stack:** Rust / tay_window(WEvent/Mods/MouseButtonIdx)/ 现有 selection + arboard 剪贴板路径。

**Spec:** docs/superpowers/specs/2026-11-09-shift-select-copy-design.md

## Global Constraints

- 只动 Shift+左键路径;普通拖选(未按 Shift)行为与现状完全一致。
- 应用收不到 Shift 拖选期间的任何指针事件(按下/移动/松开)。
- 非目标(不做):Shift+滚轮、拖动中途按 Shift 夺回、选区同步应用。
- 零拖动松开:清空选区,不复制。
- 提交直接进 main,消息风格「类别: 描述」;打包 pkgrel 52 → 53。

## Review Focus

| # | 输入/条件 | 期望行为 | 钉住的测试(Task) |
|---|---|---|---|
| 1 | Shift 拖选中 1003 移动事件 | 不进 PTY,选区扩展 | shift_select 状态机 + mouse_report 旁路(Task 1) |
| 2 | 应用先收到非 Shift 按下,拖拽中按 Shift | 应用持有拖动不变(不夺回) | 旁路只作用于按下(Task 1 测试注释钉死) |
| 3 | 快速 hide/toggle/关窗中断 Shift 拖选 | active 清零,下一次按下正常 | cancel 路径(Task 2) |
| 4 | 无鼠标模式窗格上 Shift 拖选 | 同样松开复制(WezTerm 一致) | 本地按下接线不分鼠标模式(Task 1) |
| 5 | Shift+中/右键 | 照常进应用,不触发旁路 | bypass_report 纯测试(Task 1) |
| 6 | 松开时选区已被搜索/滚动清空 | 不复制不 panic | end(has_selection=false)(Task 1) |

---

### Task 1: 旁路状态机 + 路由接线

**Files:**
- Create: `crates/tayd/src/shift_select.rs`(含模块测试)
- Modify: `crates/tayd/src/app.rs`(字段、mouse_report 旁路、本地按下/松开接线)、`crates/tayd/src/lib.rs`(mod 声明)

**Interfaces:**
- Consumes: 现有 `mouse_report`、本地 PointerButton 处理(按下 ~4412、松开 ~4421)、`copy_selection()`、`self.pointer_mods`
- Produces:
  - `pub fn bypass_report(mods: &Mods, button: MouseButtonIdx) -> bool` —— shift && button == Left
  - `pub struct ShiftSelect { active: bool }`;`ShiftSelect::new()`、`begin(&mut self, shift: bool) -> bool(是否激活)`、`active(&self) -> bool`、`end(&mut self, has_selection: bool) -> bool(是否复制)`、`cancel(&mut self)`

- [ ] **Step 1: 写失败测试**(shift_select.rs 模块测试):
  - `begin_activates_only_with_shift`:begin(true)=true 且 active;begin(false)=false 且 !active
  - `end_copies_only_active_with_selection`:active+has_selection → true 且清零;active+无选区 → false 且清零;未激活 → false
  - `cancel_clears_active`
  - `bypass_only_shift_left`:shift+Left → true;无 shift+Left、shift+Right、shift+Middle → false

- [ ] **Step 2: 运行确认失败**

Run: `cargo test -p tayd shift_select`
Expected: FAIL(模块不存在)

- [ ] **Step 3: 实现 `shift_select.rs`** 按 Interfaces。

- [ ] **Step 4: 运行确认通过** —— Expected: PASS。

- [ ] **Step 5: app.rs 接线** ——
  - 字段 `shift_select: crate::shift_select::ShiftSelect`(init `new()`);
  - `mouse_report` 按下分支首行(在 pane_cell_at 之前):`if crate::shift_select::bypass_report(&self.pointer_mods, *button) { self.shift_select.begin(true); return false; }`;移动分支首行:`if self.shift_select.active() { return false; }`;
  - 本地按下处理(设置 dragging=true 处)加 `self.shift_select.begin(self.pointer_mods.shift);`;
  - 本地松开处理:`let has_sel = matches!(self.sessions[self.active_pane].ui.sel, Some((s, e)) if s != e);` → 现有零长度清理 → `if self.shift_select.end(has_sel) { self.copy_selection(); }`。

- [ ] **Step 6: 验证** —— `cargo test -p tayd --lib` 全绿;`cargo clippy -p tayd`、`cargo fmt --check` 干净。

- [ ] **Step 7: Commit**

```bash
git add crates/tayd/src/shift_select.rs crates/tayd/src/app.rs crates/tayd/src/lib.rs
git commit -m "Shift 拖选复制: 鼠标模式开启时 Shift+左键旁路本地选区,松开复制(WezTerm 语义),事件不进应用"
```

---

### Task 2: 取消路径 + 全量回归

**Files:**
- Modify: `crates/tayd/src/app.rs`(5 处 dragging 清零点同步 cancel)

- [ ] **Step 1: 找到 5 处 `ui.dragging = false`(hide/toggle/close/zoom 等)**,每处加 `self.shift_select.cancel();`。

- [ ] **Step 2: 验证** —— `cargo test --locked --workspace` 全绿;clippy 0、fmt 干净;三套烟测(panes/focus/desktop-services)全绿。

- [ ] **Step 3: Commit**

```bash
git add crates/tayd/src/app.rs
git commit -m "Shift 拖选复制: hide/toggle/关窗等取消路径同步清 shift_select"
```

---

### Task 3: 真机验收 + 打包 pkgrel 53

**Files:**
- Modify: `packaging/arch/PKGBUILD`(pkgrel 52 → 53)、spec 状态、记忆

- [ ] **Step 1: 隔离实例冒烟** —— toggle 循环无崩溃;日志无 panic。

- [ ] **Step 2: 用户真机验收** —— 在 Claude Code 内 Shift 拖选 → 松开 → Ctrl+V 粘贴正确;普通拖选行为不变;Claude Code 自身鼠标操作(非 Shift)不受影响。

- [ ] **Step 3: 打包** —— pkgrel 53;`./tools/package-arch.sh`;包内验证;用户 `sudo pacman -U` 实测。

- [ ] **Step 4: 文档收尾 + Commit**

```bash
git add packaging/arch/PKGBUILD docs/superpowers/specs/2026-11-09-shift-select-copy-design.md
git commit -m "Shift 拖选复制: 真机验收通过; 打包: pkgrel 52 → 53"
```
