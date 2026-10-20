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
  theme = "midnight", -- 参考本地 WezTerm 自定义配色；也可选 default / midnight / light
  opacity = 0.98,      -- 背景不透明度：1 完全不透明，0 完全透明；文字保持清晰
  font = { family = "JetBrainsMono Nerd Font", size = 12.0, weight = 700 },
  height_percent = 55,
  padding = 10,
  animation_ms = 100,
  hide_on_focus_out = false, -- 点击别的窗口时仍保留，避免切换焦点就消失
  start_hidden = false,
  -- 半圆角冷色标签条；Ctrl+, 展开设置，+ 新建，齿轮设置。
  tab_bar = {
    compact = true, tab_width = 24,
    background = "#101b29", active_background = "#29445c",
    active_foreground = "#e5f2fa", inactive_foreground = "#9fb2c8",
    hover_background = "#1d3044", accent = "#96d7e5",
    icon_foreground = "#93a8be", icon_hover_foreground = "#a4dce8",
    danger_foreground = "#ee9fa7",
    clock_background = "#244155", clock_foreground = "#cae7ef",
  },
  colors = { background = "#0b111b", foreground = "#d6e5f5",
             cursor = "#96d7e5", selection = "#244258",
    ansi = { "#101722", "#de8e98", "#a5c99b", "#d4bb86", "#8faee0", "#baa5d8", "#8ccbd8", "#cbd8e7", "#71869e", "#eea4ac", "#bcdbad", "#e8cf9c", "#abc3e9", "#cfb9e9", "#a7e0e9", "#eef4fc" },
  },
  -- 可覆盖主题中的单个颜色；ansi 接受恰好 16 个 #RRGGBB。
  -- colors = { background = "#171c27", foreground = "#d8dee9" },
}
