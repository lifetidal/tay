-- Tay 日常配置。修改主题、字体、透明度和窗口内快捷键后自动生效。
-- 全局呼出快捷键修改需要重启新版，并在桌面授权窗口确认。
local keys = {
  { key = "t",        mods = "CTRL",       action = "new-tab" },
  { key = "w",        mods = "CTRL",       action = "close-tab" },
  { key = "t",        mods = "ALT",        action = "new-tab" },
  { key = "[",        mods = "ALT",        action = "previous-tab" },
  { key = "]",        mods = "ALT",        action = "next-tab" },
  { key = "t",        mods = "CTRL|SHIFT", action = "new-tab" },
  { key = "w",        mods = "CTRL|SHIFT", action = "close-tab" },
  { key = "Tab",      mods = "CTRL",       action = "next-tab" },
  { key = "Tab",      mods = "CTRL|SHIFT", action = "previous-tab" },
  { key = "PageDown", mods = "CTRL",       action = "next-tab" },
  { key = "PageUp",   mods = "CTRL",       action = "previous-tab" },
  { key = "c",        mods = "CTRL|SHIFT", action = "copy" },
  { key = "v",        mods = "CTRL|SHIFT", action = "paste" },
  { key = "Insert",   mods = "SHIFT",      action = "paste" },
  { key = "=",        mods = "CTRL",       action = "font-increase" },
  { key = "+",        mods = "CTRL|SHIFT", action = "font-increase" },
  { key = "+",        mods = "CTRL",       action = "font-increase" },
  { key = "-",        mods = "CTRL",       action = "font-decrease" },
  { key = "0",        mods = "CTRL",       action = "font-reset" },
  { key = ",",        mods = "CTRL",       action = "settings" },
  { key = "m",        mods = "CTRL|SHIFT", action = "maximize" },
  { key = "`",        mods = "CTRL",       action = "hide" },
  { key = "F12",      mods = "",           action = "hide" },
}
-- Alt+1 … Alt+9 直接跳到相应标签页；Ctrl+C / Ctrl+V 留给 Shell/TUI。
for i = 1, 9 do
  keys[#keys + 1] = { key = tostring(i), mods = "ALT", action = "tab-" .. i }
end

return {
  shell = "/bin/bash",
  global_shortcut = "CTRL+grave", -- Ctrl+` 全局呼出/隐藏（不需要笔记本 Fn 键）
  keys = keys,
  theme = "wezterm", -- 参考本地 WezTerm 自定义配色；也可选 default / midnight / light
  opacity = 0.94,      -- 背景不透明度：1 完全不透明，0 完全透明；文字保持清晰
  font = { family = "JetBrainsMono Nerd Font", size = 12.0, weight = 500 },
  height_percent = 55,
  padding = 8,
  animation_ms = 100,
  hide_on_focus_out = false, -- 点击别的窗口时仍保留，避免切换焦点就消失
  start_hidden = false,
  -- 可覆盖主题中的单个颜色；ansi 接受恰好 16 个 #RRGGBB。
  -- colors = { background = "#171c27", foreground = "#d8dee9" },
}
