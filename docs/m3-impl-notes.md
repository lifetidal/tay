# M3 实施笔记(2026-10-16)

M3 = 体验差异化(§9):OSC 133 命令块、QuickSelect、搜索/复制模式、主题热重载。

## 已完成:OSC 133 命令块(fish 零配置)

- **关键决策:自研 PTY 事件循环**(crates/tayd/src/ptyloop.rs,~230 行)替换
  alacritty_terminal::event_loop::EventLoop。原因:原版事件循环把 PTY 字节
  直接喂给 vte parser,前端看不到输出流,无法消费 OSC 133;且 unix Pty 的
  signals 字段私有,无法透明包装。自研后行为等价(poll PTY + 唤醒管道、
  DEC 2026 同步超时、子进程退出通知),读取路径归前端。
- OSC 133 解析:字节级状态机(跨 read 分块、BEL/ST 双终止、非 133 OSC
  忽略),单测 4 个。A → 记录光标网格行;B/C 忽略;D → 退出码。
- 渲染:非零退出码在对应提示符行尾画红色 `✗ N` 徽章(替换行尾空格,
  tay-render 行尾 splice)。
- 快捷键:Ctrl+Shift+↑/↓ 跳转提示符(scroll_to_point);Ctrl+Shift+O
  复制上一条命令的输出(bounds_to_string)。
- fish 4.9 实测:A/B/C/D 全序列捕获;`ls /nonexistent` → `✗ 2` 徽章 ✓。

## 已完成:QuickSelect(§8.3)

- Ctrl+Shift+F 触发:扫描可见网格,匹配 URL / git SHA / 路径 三类
  (regex,同排重叠去重),分配 a-z、aa-zz 标签
- 渲染:overlay 层(tay-render)—— 标签字符替换 + teal 底色块
- 按键:按标签字母即完成(复制到剪贴板;URL 额外 xdg-open);
  Esc / 无效前缀取消
- 验证:TAY_QUICKSELECT 环境变量自动触发,8 匹配项截图确认

## 已完成:搜索/复制模式 + 主题热重载

- **vi 复制模式**(Ctrl+Shift+Space):h/j/k/l、0/$/^、w/b/e、G/g 动作
  (term 内置 vi_motion);y 复制选区退出;/ 增量搜索(RegexSearch,
  滚动回退全库搜索 + scroll_to_point 跳转);n/N 下一/上一命中;
  Esc/q 退出;vi 模式光束光标反馈
- **主题热重载**:notify 监视 config.toml → mtime 变化 → 重载;
  颜色即时生效(实测 2 秒内),字体变化提示重启
- 验证:TAY_VI 环境变量(光束光标 59px 截图确认);热重载截图前后色
  (#1A1030 → #003020)

## 已完成:命令块折叠(§8.4 最后一项)

- 触发:**点击退出码徽章** 或 **Ctrl+Shift+H**(折叠光标所在块的输出)
- 折叠区间 = 相邻提示符之间的输出行,压缩为一行 `⋯ N 行已折叠`
- 显示行 → 网格行重映射(row_map):光标/选区/徽章/QuickSelect
  全链路折叠感知;折叠时关闭选区与 QuickSelect
- 验证:TAY_FOLD 自动折叠 + 行内容 dump —— 输出行消失、
  标记行与新提示符正确(截图像素密度分布确认)

M3 全部完成。
- 性能:OSC 扫描在 PTY 读取路径上,3000 行滚动 1030fps(vs 之前 1123fps,
  ~10% 开销,可接受;后续可按「只在非 133 上下文跳过」优化)
