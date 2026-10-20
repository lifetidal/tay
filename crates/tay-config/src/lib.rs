//! Tay 配置:Lua 脚本(主)+ TOML(旧格式,自动迁移)。
//!
//! WezTerm 风格:`~/.config/tay/config.lua` 是一段脚本,`return`
//! 一个配置表;缺字段用默认值,类型错误/越界整体拒绝(热重载
//! 时沿用上次有效配置)。沙箱:仅加载安全标准库(无 os/io/
//! debug/package —— 配置不该执行任意副作用)。
//!
//! 零配置哲学:没有配置文件时全部字段有可用默认值。旧
//! `config.toml` 首次启动时自动迁移为 config.lua(保留值)。
//! 配置只从 config.lua 读取；旧 TOML 外观仅一次性迁移到 Lua 后归档。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// RGB 颜色(0–255),TOML 里写作 `"#rrggbb"`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    pub fn from_hex(value: &str) -> Option<Self> {
        parse_hex(value)
    }

    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self(r, g, b)
    }

    /// 转为 cosmic-text / glyphon 使用的 u32(0xRRGGBB)。
    pub const fn to_u32(self) -> u32 {
        ((self.0 as u32) << 16) | ((self.1 as u32) << 8) | self.2 as u32
    }

    pub const fn to_array(self) -> [f32; 4] {
        [
            self.0 as f32 / 255.0,
            self.1 as f32 / 255.0,
            self.2 as f32 / 255.0,
            1.0,
        ]
    }
}

impl Serialize for Rgb {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format!("#{:02x}{:02x}{:02x}", self.0, self.1, self.2))
    }
}

impl<'de> Deserialize<'de> for Rgb {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        parse_hex(&s).ok_or_else(|| {
            serde::de::Error::custom(format!("颜色必须是 \"#rrggbb\" 格式,得到: {s:?}"))
        })
    }
}

fn parse_hex(s: &str) -> Option<Rgb> {
    let s = s.strip_prefix('#')?;
    // 字节安全:先确认全部是 ASCII 十六进制,再按字节切片 ——
    // 多字节 UTF-8(如 "#aéé" 恰为 6 字节)不满足此条件,直接拒绝,
    // 不会在字符边界处 panic。
    if s.len() != 6 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let v = |i: usize| u8::from_str_radix(&s[i..i + 2], 16).ok();
    Some(Rgb(v(0)?, v(2)?, v(4)?))
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct Font {
    /// 字体族名称,cosmic-text 的 fontdb 会做回退(CJK / Nerd Font)。
    pub family: String,
    /// 字号(pt)。
    pub size: f32,
    /// CSS/OpenType weight: 400 Regular, 500 Medium, 700 Bold.
    pub weight: u16,
}

impl Default for Font {
    fn default() -> Self {
        Self {
            family: "monospace".into(),
            size: 12.0,
            weight: 400,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct Colors {
    pub foreground: Rgb,
    pub background: Rgb,
    pub cursor: Rgb,
    pub selection: Rgb,
    pub ansi: [Rgb; 16],
}

impl Default for Colors {
    fn default() -> Self {
        Self {
            foreground: Rgb::new(0xd4, 0xd4, 0xd4),
            background: Rgb::new(0x0f, 0x11, 0x14),
            cursor: Rgb::new(0x00, 0xa8, 0x84),
            selection: Rgb::new(0x39, 0x5e, 0x9c),
            ansi: [
                Rgb(0, 0, 0),
                Rgb(205, 0, 0),
                Rgb(0, 205, 0),
                Rgb(205, 205, 0),
                Rgb(0, 0, 238),
                Rgb(205, 0, 205),
                Rgb(0, 205, 205),
                Rgb(229, 229, 229),
                Rgb(127, 127, 127),
                Rgb(255, 0, 0),
                Rgb(0, 255, 0),
                Rgb(255, 255, 0),
                Rgb(92, 92, 255),
                Rgb(255, 0, 255),
                Rgb(0, 255, 255),
                Rgb(255, 255, 255),
            ],
        }
    }
}

impl Colors {
    pub fn preset(name: &str) -> Option<Self> {
        let (bg, fg, cursor, selection, palette) = match name {
            "default" => return Some(Self::default()),
            "midnight" => (
                "#171c27",
                "#d8dee9",
                "#58a6ff",
                "#334d72",
                [
                    "#202632", "#ed8796", "#a6da95", "#eed49f", "#8aadf4", "#c6a0f6", "#8bd5ca",
                    "#cad3f5", "#6e738d", "#f5a0b0", "#bce7ac", "#f8dfb0", "#a7c1ff", "#d8b8ff",
                    "#a5ebe1", "#ffffff",
                ],
            ),
            // Matched to the local WezTerm colors/custom.lua palette.
            "wezterm" => (
                "#1f1f28",
                "#cdd6f4",
                "#f5e0dc",
                "#585b70",
                [
                    "#45475a", "#f38ba8", "#a6e3a1", "#f9e2af", "#89b4fa", "#f5c2e7", "#94e2d5",
                    "#bac2de", "#585b70", "#f38ba8", "#a6e3a1", "#f9e2af", "#89b4fa", "#f5c2e7",
                    "#94e2d5", "#a6adc8",
                ],
            ),
            "light" => (
                "#f5f6fa",
                "#202632",
                "#2266dd",
                "#b8d2ff",
                [
                    "#202632", "#a82735", "#28723d", "#8c6316", "#2259aa", "#8543a0", "#18727b",
                    "#c0c6d0", "#636c7b", "#c52c3e", "#328c49", "#a47a1c", "#286bc9", "#a252bf",
                    "#228b96", "#ffffff",
                ],
            ),
            _ => return None,
        };
        Some(Self {
            background: parse_hex(bg)?,
            foreground: parse_hex(fg)?,
            cursor: parse_hex(cursor)?,
            selection: parse_hex(selection)?,
            ansi: palette.map(|c| parse_hex(c).unwrap()),
        })
    }
    pub fn indexed(&self, index: u8) -> Rgb {
        match index {
            0..=15 => self.ansi[index as usize],
            16..=231 => {
                let i = index - 16;
                let levels = [0, 95, 135, 175, 215, 255];
                Rgb(
                    levels[(i / 36) as usize],
                    levels[((i % 36) / 6) as usize],
                    levels[(i % 6) as usize],
                )
            }
            _ => {
                let v = 8 + (index - 232) * 10;
                Rgb(v, v, v)
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct KeyBinding {
    pub key: String,
    pub mods: String,
    pub action: String,
}
impl KeyBinding {
    pub fn canonical_key(key: &str) -> String {
        match key.to_ascii_lowercase().as_str() {
            " " => "space".into(),
            "leftarrow" => "left".into(),
            "rightarrow" => "right".into(),
            "uparrow" => "up".into(),
            "downarrow" => "down".into(),
            "esc" => "escape".into(),
            "return" => "enter".into(),
            other => other.into(),
        }
    }
    pub fn modifier_mask(&self) -> Result<u8, String> {
        if self.mods.trim().eq_ignore_ascii_case("NONE") {
            return Ok(0);
        }
        let mut mask = 0;
        for part in self
            .mods
            .split('|')
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            mask |= match part.to_ascii_uppercase().as_str() {
                "CTRL" => 1,
                "SHIFT" => 2,
                "ALT" => 4,
                "SUPER" => 8,
                _ => return Err(format!("未知修饰键: {part}")),
            };
        }
        Ok(mask)
    }
}
fn default_keys() -> Vec<KeyBinding> {
    let mut keys: Vec<_> = [
        ("t", "CTRL", "new-tab"),
        ("w", "CTRL", "close-tab"),
        ("t", "ALT", "new-tab"),
        ("[", "ALT", "previous-tab"),
        ("]", "ALT", "next-tab"),
        ("t", "CTRL|SHIFT", "new-tab"),
        ("w", "CTRL|SHIFT", "close-tab"),
        ("Tab", "CTRL", "next-tab"),
        ("Tab", "CTRL|SHIFT", "previous-tab"),
        ("PageDown", "CTRL", "next-tab"),
        ("PageUp", "CTRL", "previous-tab"),
        ("c", "CTRL|SHIFT", "copy"),
        ("v", "CTRL|SHIFT", "paste"),
        ("Insert", "SHIFT", "paste"),
        ("m", "CTRL|SHIFT", "maximize"),
        ("=", "CTRL", "font-increase"),
        ("+", "CTRL|SHIFT", "font-increase"),
        ("+", "CTRL", "font-increase"),
        ("-", "CTRL", "font-decrease"),
        ("0", "CTRL", "font-reset"),
        (",", "CTRL", "settings"),
        ("F12", "", "hide"),
        ("`", "CTRL", "hide"),
    ]
    .into_iter()
    .map(|(key, mods, action)| KeyBinding {
        key: key.into(),
        mods: mods.into(),
        action: action.into(),
    })
    .collect();
    keys.extend((1..=9).map(|n| KeyBinding {
        key: n.to_string(),
        mods: "ALT".into(),
        action: format!("tab-{n}"),
    }));
    keys
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub font: Font,
    pub colors: Colors,
    pub keys: Vec<KeyBinding>,
    pub disable_default_key_bindings: bool,
    pub global_shortcut: String,
    pub theme: String,
    /// 启动的 shell;空串 = 取 `$SHELL`(再兜底 `/bin/sh`)。
    pub shell: String,
    /// 单元格内边距(px,每条边),留白让 CJK/图标不贴框。
    pub padding: f32,
    /// 默认背景不透明度 0.0–1.0,文字/控件保持不透明;1.0 = 完全不透明(Wayland 模糊是增强项,见 DESIGN §6.4)。
    pub opacity: f32,
    /// 呼出时终端高度占输出高度的百分比。
    pub height_percent: u32,
    /// 呼出/收起动画时长(ms),0 = 无动画。
    pub animation_ms: u32,
    /// 失焦自动隐藏(Wayland 上即 keyboard leave;见 DESIGN §6.3)。
    pub hide_on_focus_out: bool,
    /// daemon 启动时隐藏(M1 常驻模型)。
    pub start_hidden: bool,
    /// 呼出所在的输出(显示器名,如 "DP-1";空 = 合成器默认)。
    pub output: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            font: Font::default(),
            colors: Colors::default(),
            keys: default_keys(),
            disable_default_key_bindings: false,
            global_shortcut: "F12".into(),
            theme: "default".into(),
            shell: String::new(),
            padding: 3.0,
            opacity: 1.0,
            height_percent: 45,
            animation_ms: 150,
            hide_on_focus_out: true,
            start_hidden: true,
            output: String::new(),
        }
    }
}

impl Config {
    /// 配置文件路径:$XDG_CONFIG_HOME/tay/config.lua(Lua,主)。
    pub fn config_path() -> PathBuf {
        Self::config_dir().join("config.lua")
    }

    /// 旧格式路径(config.toml):仅用于一次性迁移。
    pub fn toml_path() -> PathBuf {
        Self::config_dir().join("config.toml")
    }

    fn config_dir() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("tay")
    }

    /// 读取配置:默认值 + 配置文件覆盖。文件缺失或损坏都退回默认值
    /// (损坏时报错日志但不阻止启动——呼出可用性优先)。
    /// 热重载应改用 [`Config::try_load`],失败时保留旧配置。
    pub fn load() -> Config {
        match Self::try_load() {
            Ok(cfg) => cfg,
            Err(e) => {
                eprintln!("警告:配置加载失败,使用默认配置: {e}");
                Config::default()
            }
        }
    }

    /// 严格加载:config.lua 求值;缺失时迁移旧 config.toml;
    /// 两者皆无 → 默认配置。解析失败或字段越界都返回错误信息,
    /// 不回退默认值。热重载用:失败时调用方保留上次有效配置。
    pub fn try_load() -> Result<Config, String> {
        Self::try_load_at(&Self::config_dir())
    }

    fn try_load_at(dir: &Path) -> Result<Config, String> {
        let mut cfg = Self::load_base_at(dir)?;
        let legacy = dir.join("appearance.toml");
        if legacy.exists() {
            let mut migrated = cfg.clone();
            match migrated.apply_appearance(&legacy) {
                Ok(()) => {
                    cfg = migrated;
                    match cfg.save_appearance_at(&dir.join("config.lua")) {
                        Ok(()) => {
                            if let Err(e) = archive_legacy(&legacy) {
                                eprintln!("警告: {e}; 已保留有效 Lua 配置");
                            }
                        }
                        Err(e) => eprintln!(
                            "警告: 外观迁移暂未写入 Lua: {e}; 保留旧文件和当前设置，稍后重试"
                        ),
                    }
                }
                Err(e) => eprintln!("警告: 忽略无效的旧外观文件: {e}; 使用有效 Lua 配置"),
            }
        }
        let old_config = dir.join("config.toml");
        if old_config.exists()
            && dir.join("config.lua").exists()
            && let Err(e) = archive_legacy(&old_config)
        {
            eprintln!("警告: {e}; 已保留有效 Lua 配置");
        }

        Ok(cfg)
    }

    fn load_base_at(dir: &Path) -> Result<Config, String> {
        let path = dir.join("config.lua");
        if path.exists() {
            let text = std::fs::read_to_string(&path)
                .map_err(|e| format!("读取 {} 失败: {e}", path.display()))?;
            return Self::from_lua(&text).map_err(|e| format!("{} 求值失败: {e}", path.display()));
        }
        // 旧 TOML → 一次性迁移(保留值,原文件不动)。
        let toml_path = dir.join("config.toml");
        if toml_path.exists() {
            let text = std::fs::read_to_string(&toml_path)
                .map_err(|e| format!("读取 {} 失败: {e}", toml_path.display()))?;
            let cfg: Config =
                toml::from_str(&text).map_err(|e| format!("旧 config.toml 解析失败: {e}"))?;
            cfg.validate()?;
            cfg.write_lua_at(&path)?;
            eprintln!("配置已从 config.toml 迁移到 {}", path.display());
            return Ok(cfg);
        }
        Ok(Config::default())
    }

    /// Lua 配置求值:脚本 `return { ... }` 一个表,字段逐一映射,
    /// 缺失用默认、类型错误/越界整体拒绝。沙箱:仅安全标准库
    /// (无 os/io/debug/package)。
    pub fn from_lua(text: &str) -> Result<Config, String> {
        if text.len() > 1024 * 1024 {
            return Err("配置脚本超过 1 MiB".into());
        }
        let libs =
            mlua::StdLib::TABLE | mlua::StdLib::STRING | mlua::StdLib::MATH | mlua::StdLib::UTF8;
        let lua =
            mlua::Lua::new_with(libs, mlua::LuaOptions::default()).map_err(|e| e.to_string())?;
        lua.set_memory_limit(8 * 1024 * 1024)
            .map_err(|e| e.to_string())?;
        // Prevent catching the instruction-limit error or starting unhooked coroutines.
        for name in ["pcall", "xpcall", "load", "loadfile", "dofile"] {
            lua.globals()
                .set(name, mlua::Value::Nil)
                .map_err(|e| e.to_string())?;
        }
        let remaining = std::cell::Cell::new(100u32);
        let start = std::time::Instant::now();
        lua.set_hook(
            mlua::HookTriggers::new().every_nth_instruction(1000),
            move |_, _| {
                let left = remaining.get();
                if left == 0 || start.elapsed() > std::time::Duration::from_millis(25) {
                    return Err(mlua::Error::RuntimeError("配置超出执行预算".into()));
                }
                remaining.set(left - 1);
                Ok(mlua::VmState::Continue)
            },
        )
        .map_err(|e| e.to_string())?;
        let value: mlua::Value = lua.load(text).eval().map_err(|e| e.to_string())?;
        let t = match value {
            mlua::Value::Table(t) => t,
            _ => return Err("配置必须 `return { ... }` 一个表".into()),
        };
        let mut cfg = Config::default();
        if let Some(name) = get_str(&t, "theme")? {
            cfg.colors = Colors::preset(&name)
                .ok_or_else(|| format!("未知主题: {name}; 可选 default/midnight/wezterm/light"))?;
            cfg.theme = name;
        }
        if let Some(trigger) = get_str(&t, "global_shortcut")? {
            cfg.global_shortcut = trigger;
        }
        if let Some(disable) = get_bool(&t, "disable_default_key_bindings")? {
            cfg.disable_default_key_bindings = disable;
        }
        if cfg.disable_default_key_bindings {
            cfg.keys.clear();
        }
        if let Some(keys) = get_table(&t, "keys")? {
            let mut custom_seen = std::collections::BTreeSet::new();
            for entry in keys.sequence_values::<mlua::Table>() {
                let entry = entry.map_err(|e| e.to_string())?;
                let binding = KeyBinding {
                    key: get_str(&entry, "key")?.ok_or("keys.key 缺失")?,
                    mods: get_str(&entry, "mods")?.unwrap_or_default(),
                    action: get_str(&entry, "action")?.ok_or("keys.action 缺失")?,
                };
                let identity = (
                    KeyBinding::canonical_key(&binding.key),
                    binding.modifier_mask()?,
                );
                if !custom_seen.insert(identity.clone()) {
                    return Err("存在重复快捷键".into());
                }
                cfg.keys.retain(|b| {
                    (
                        KeyBinding::canonical_key(&b.key),
                        b.modifier_mask().unwrap_or(255),
                    ) != identity
                });
                cfg.keys.push(binding);
            }
        }

        if let Some(v) = get_str(&t, "shell")? {
            cfg.shell = v;
        }
        if let Some(v) = get_uint(&t, "height_percent")? {
            cfg.height_percent = v;
        }
        if let Some(v) = get_uint(&t, "animation_ms")? {
            cfg.animation_ms = v;
        }
        if let Some(v) = get_bool(&t, "hide_on_focus_out")? {
            cfg.hide_on_focus_out = v;
        }
        if let Some(v) = get_bool(&t, "start_hidden")? {
            cfg.start_hidden = v;
        }
        if let Some(v) = get_str(&t, "output")? {
            cfg.output = v;
        }
        if let Some(v) = get_num(&t, "padding")? {
            cfg.padding = v as f32;
        }
        if let Some(v) = get_num(&t, "opacity")? {
            cfg.opacity = v as f32;
        }
        if let Some(ft) = get_table(&t, "font")? {
            if let Some(v) = get_str(&ft, "family")? {
                cfg.font.family = v;
            }
            if let Some(v) = get_uint(&ft, "weight")? {
                cfg.font.weight = u16::try_from(v).map_err(|_| "font.weight 越界")?;
            }
            if let Some(v) = get_num(&ft, "size")? {
                cfg.font.size = v as f32;
            }
        }
        if let Some(ct) = get_table(&t, "colors")? {
            if let Some(v) = get_color(&ct, "foreground")? {
                cfg.colors.foreground = v;
            }
            if let Some(v) = get_color(&ct, "background")? {
                cfg.colors.background = v;
            }
            if let Some(v) = get_color(&ct, "cursor")? {
                cfg.colors.cursor = v;
            }
            if let Some(v) = get_color(&ct, "selection")? {
                cfg.colors.selection = v;
            }
        }
        if let Some(ct) = get_table(&t, "colors")?
            && let Some(ansi) = get_table(&ct, "ansi")?
        {
            let colors = ansi
                .sequence_values::<String>()
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| e.to_string())?;
            if colors.len() != 16 {
                return Err("colors.ansi 需要 16 种颜色".into());
            }
            for (i, color) in colors.iter().enumerate() {
                cfg.colors.ansi[i] = parse_hex(color).ok_or("ansi 颜色需要 #RRGGBB")?;
            }
        }
        cfg.validate()?;
        Ok(cfg)
    }

    /// 范围校验:拒绝 NaN/Inf 与越界值(配置脚本允许 `nan`/`inf`
    /// 字面量,会让布局数学全部失效;越界值通常意味着单位误解,
    /// 明确报错好过静默错乱)。
    fn validate(&self) -> Result<(), String> {
        let mut errs: Vec<String> = Vec::new();
        let mut check = |ok: bool, msg: &str| {
            if !ok {
                errs.push(msg.to_string());
            }
        };
        check(
            self.font.size.is_finite() && (1.0..=200.0).contains(&self.font.size),
            "font.size 需为 1–200 的有限数(pt)",
        );
        check(
            (100..=900).contains(&self.font.weight),
            "font.weight 需为 100–900",
        );
        check(
            self.padding.is_finite() && (0.0..=100.0).contains(&self.padding),
            "padding 需为 0–100 的有限数(px)",
        );
        check(
            self.opacity.is_finite() && (0.0..=1.0).contains(&self.opacity),
            "opacity 需为 0–1",
        );
        check(
            (1..=100).contains(&self.height_percent),
            "height_percent 需为 1–100",
        );
        check(self.animation_ms <= 5000, "animation_ms 需为 0–5000");
        check(
            !self.global_shortcut.trim().is_empty(),
            "global_shortcut 不能为空",
        );
        let mut seen = std::collections::BTreeSet::new();
        for binding in &self.keys {
            let key = KeyBinding::canonical_key(&binding.key);
            check(
                binding.key.chars().count() == 1
                    || [
                        "tab",
                        "enter",
                        "escape",
                        "space",
                        "insert",
                        "pageup",
                        "pagedown",
                        "left",
                        "right",
                        "up",
                        "down",
                        "home",
                        "end",
                        "backspace",
                        "delete",
                    ]
                    .contains(&key.as_str())
                    || key
                        .strip_prefix('f')
                        .and_then(|s| s.parse::<u8>().ok())
                        .is_some_and(|n| (1..=12).contains(&n)),
                "keys.key 不受支持",
            );
            let mask = binding.modifier_mask();
            check(mask.is_ok(), "keys.mods 仅支持 CTRL|SHIFT|ALT|SUPER");
            check(seen.insert((key, mask.unwrap_or(255))), "存在重复快捷键");
            check(
                [
                    "new-tab",
                    "close-tab",
                    "next-tab",
                    "previous-tab",
                    "copy",
                    "paste",
                    "settings",
                    "font-increase",
                    "font-decrease",
                    "font-reset",
                    "hide",
                    "maximize",
                    "none",
                ]
                .contains(&binding.action.as_str())
                    || binding
                        .action
                        .strip_prefix("tab-")
                        .and_then(|s| s.parse::<usize>().ok())
                        .is_some_and(|n| (1..=9).contains(&n)),
                "keys.action 不受支持",
            );
        }
        if errs.is_empty() {
            Ok(())
        } else {
            Err(errs.join("; "))
        }
    }

    /// Reset zoom to the Lua/TOML base size, ignoring the saved UI zoom overlay.
    pub fn base_font_size() -> Result<f32, String> {
        let path = Self::config_path();
        match std::fs::read_to_string(path) {
            Ok(text) => Self::from_lua(lua_base(&text)?).map(|c| c.font.size),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Self::load_base_at(&Self::config_dir()).map(|c| c.font.size)
            }
            Err(e) => Err(e.to_string()),
        }
    }

    pub fn binding_hint(&self, action: &str) -> String {
        self.keys
            .iter()
            .find(|b| b.action == action)
            .map(|b| {
                let prefix = b
                    .mods
                    .split('|')
                    .filter(|m| !m.is_empty() && !m.eq_ignore_ascii_case("NONE"))
                    .map(|m| match m.trim().to_ascii_uppercase().as_str() {
                        "CTRL" => "Ctrl",
                        "SHIFT" => "Shift",
                        "ALT" => "Alt",
                        _ => "Super",
                    })
                    .collect::<Vec<_>>()
                    .join("+");
                if prefix.is_empty() {
                    b.key.clone()
                } else {
                    format!("{prefix}+{}", b.key)
                }
            })
            .unwrap_or_else(|| "unbound".into())
    }

    /// 规范化 Lua 模板(模板生成/迁移共用同一序列化,保证
    /// 写出的文件必然可重新加载 —— 有测试锁 roundtrip)。
    pub fn to_lua(&self) -> String {
        format!(
            r#"-- Tay 配置(WezTerm 风格:脚本返回配置表;保存后自动热重载)。
-- 热重载失败(语法/类型/越界)会沿用上次有效配置。
-- 即时生效:height_percent / animation_ms / hide_on_focus_out /
--          padding / opacity / colors / font
-- 需重启 tayd 才生效:shell / output / start_hidden
-- 界面外观调整也写入本文件；原始脚本、注释和逻辑保留。

return {{
  theme = {theme:?},
  global_shortcut = {shortcut:?},
  disable_default_key_bindings = {disable},
  keys = {{ {keys} }},
  -- 启动的 shell;空字符串 = 取 $SHELL(需重启)
  shell = {shell:?},
  -- 呼出高度占输出百分比(1–100)
  height_percent = {hp},
  -- 呼出/收起动画时长(ms),0 = 无动画
  animation_ms = {anim},
  -- 失焦自动隐藏
  hide_on_focus_out = {hfo},
  -- daemon 启动时隐藏(需重启)
  start_hidden = {sh},
  -- 呼出所在输出(如 "DP-1");空 = 合成器默认(需重启)
  output = {out:?},

  -- 单元格内边距(px)
  padding = {pad},
  -- 默认背景不透明度 0–1（文字和控件保持清晰）
  opacity = {op},

  -- 字体(即时生效)
  font = {{ family = {ff:?}, size = {fs}, weight = {fw} }},

  -- 配色(即时生效;操作栏自动跟随)
  colors = {{
    foreground = {cfg:?},
    background = {cbg:?},
    cursor = {cc:?},
    selection = {csel:?},
    ansi = {{ {ansi} }},
  }},
}}
"#,
            theme = self.theme,
            shortcut = self.global_shortcut,
            disable = self.disable_default_key_bindings,
            keys = self
                .keys
                .iter()
                .map(|k| format!(
                    "{{key={:?}, mods={:?}, action={:?}}}",
                    k.key, k.mods, k.action
                ))
                .collect::<Vec<_>>()
                .join(",\n"),
            ansi = self
                .colors
                .ansi
                .iter()
                .map(|c| format!("\"#{:02x}{:02x}{:02x}\"", c.0, c.1, c.2))
                .collect::<Vec<_>>()
                .join(","),
            shell = self.shell,
            hp = self.height_percent,
            anim = self.animation_ms,
            hfo = self.hide_on_focus_out,
            sh = self.start_hidden,
            out = self.output,
            pad = self.padding,
            op = self.opacity,
            ff = self.font.family,
            fs = self.font.size,
            fw = self.font.weight,
            cfg = format!(
                "#{:02x}{:02x}{:02x}",
                self.colors.foreground.0, self.colors.foreground.1, self.colors.foreground.2
            ),
            cbg = format!(
                "#{:02x}{:02x}{:02x}",
                self.colors.background.0, self.colors.background.1, self.colors.background.2
            ),
            cc = format!(
                "#{:02x}{:02x}{:02x}",
                self.colors.cursor.0, self.colors.cursor.1, self.colors.cursor.2
            ),
            csel = format!(
                "#{:02x}{:02x}{:02x}",
                self.colors.selection.0, self.colors.selection.1, self.colors.selection.2
            ),
        )
    }

    /// 原子写盘(临时文件 + rename)。
    fn write_lua_at(&self, path: &Path) -> Result<(), String> {
        write_lua_text(path, &self.to_lua())
    }

    /// Persist UI changes in the only active configuration file, preserving
    /// handwritten source in a function and updating one visible Lua block.
    pub fn save_appearance(&self) -> Result<(), String> {
        self.save_appearance_at(&Self::config_path())
    }

    /// Apply only the UI fields that changed, keeping newer manual Lua edits.
    pub fn save_appearance_delta(&self, previous: &Config) -> Result<(), String> {
        self.save_appearance_delta_at(previous, &Self::config_path())
    }

    fn save_appearance_delta_at(&self, previous: &Config, path: &Path) -> Result<(), String> {
        let original = read_lua_source(path)?;
        let fresh = Self::from_lua(&original)?;
        let mut merged = toml::Value::try_from(fresh).map_err(|e| e.to_string())?;
        let desired = toml::Value::try_from(self).map_err(|e| e.to_string())?;
        let old = toml::Value::try_from(previous).map_err(|e| e.to_string())?;
        for key in ["font", "colors"] {
            for (field, value) in desired[key].as_table().ok_or("外观表无效")? {
                if old[key].get(field) != Some(value) {
                    merged[key]
                        .as_table_mut()
                        .ok_or("外观表无效")?
                        .insert(field.clone(), value.clone());
                }
            }
        }
        if desired["opacity"] != old["opacity"] {
            merged["opacity"] = desired["opacity"].clone();
        }
        let config: Config = merged.try_into().map_err(|e| e.to_string())?;
        config.save_appearance_source(path, &original)
    }

    fn save_appearance_at(&self, path: &Path) -> Result<(), String> {
        let original = read_lua_source(path)?;
        self.save_appearance_source(path, &original)
    }

    fn save_appearance_source(&self, path: &Path, original: &str) -> Result<(), String> {
        self.validate()?;
        let parent = path.parent().ok_or("配置路径无效")?;
        let parsed = Self::from_lua(original)?;
        let source = lua_base(original)?;
        let tail = lua_user_tail(original, &parsed)?;
        let baseline = if tail.is_empty() {
            source.to_string()
        } else {
            format!("{LUA_UI_PREFIX}{source}{LUA_UI_END}{tail}")
        };
        let base = Self::from_lua(&baseline)?;
        let current = toml::Value::try_from(self).map_err(|e| e.to_string())?;
        let base = toml::Value::try_from(base).map_err(|e| e.to_string())?;
        let mut assignments = String::new();
        for key in ["font", "colors"] {
            let mut values = String::new();
            for (field, value) in current[key].as_table().ok_or("外观表无效")? {
                if base[key].get(field) != Some(value) {
                    values.push_str(&format!(
                        "__tay_config.{key}.{field} = {}\n",
                        lua_value(value)?
                    ));
                }
            }
            if !values.is_empty() {
                assignments.push_str(&format!(
                    "__tay_config.{key} = __tay_config.{key} or {{}}\n"
                ));
                assignments.push_str(&values);
            }
        }
        if current["opacity"] != base["opacity"] {
            assignments.push_str(&format!("__tay_config.opacity = {}\n", self.opacity));
        }
        let tail = if tail.is_empty() {
            "return __tay_config\n"
        } else {
            &tail
        };
        let manual = tail
            .strip_suffix("return __tay_config\n")
            .ok_or("Lua 文件结尾已手动改写，保留文件不覆盖；请保留最后的 return __tay_config 行")?;
        let text = if assignments.is_empty() {
            if manual.is_empty() {
                source.to_string()
            } else {
                format!("{LUA_UI_PREFIX}{source}{LUA_UI_END}{tail}")
            }
        } else {
            format!(
                "{LUA_UI_PREFIX}{source}{LUA_UI_END}{manual}{LUA_FIELDS_BEGIN}{assignments}{LUA_FIELDS_END}return __tay_config\n"
            )
        };
        Self::from_lua(&text)?;
        if text == original && path.exists() {
            return Ok(());
        }
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        if path.exists() {
            let backup = parent.join("config.lua.before-ui.bak");
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(backup)
            {
                Ok(mut file) => {
                    use std::io::Write;
                    file.set_permissions(
                        std::fs::metadata(path)
                            .map_err(|e| e.to_string())?
                            .permissions(),
                    )
                    .map_err(|e| e.to_string())?;
                    file.write_all(original.as_bytes())
                        .map_err(|e| e.to_string())?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e.to_string()),
            }
        }
        if read_lua_source(path)? != original {
            return Err("Lua 配置在保存期间发生变化，请重试；未覆盖手动修改".into());
        }
        write_lua_text_checked(path, &text, Some(original))
    }

    /// Legacy input only; the result is migrated into config.lua and archived.
    fn apply_appearance(&mut self, path: &Path) -> Result<(), String> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.to_string()),
        };
        let overlay: toml::Table =
            toml::from_str(&text).map_err(|e| format!("appearance.toml: {e}"))?;
        let mut merged = toml::Value::try_from(&*self).map_err(|e| e.to_string())?;
        for (key, value) in overlay {
            if key == "font" || key == "colors" {
                let fields = value.as_table().ok_or("外观字段应为表")?;
                let target = merged[&key].as_table_mut().ok_or("外观字段应为表")?;
                for (name, value) in fields {
                    if !target.contains_key(name) {
                        return Err(format!("未知外观字段: {key}.{name}"));
                    }
                    target.insert(name.clone(), value.clone());
                }
            } else if key == "opacity" {
                merged["opacity"] = value;
            } else {
                return Err(format!("未知外观字段: {key}"));
            }
        }
        let restored: Config = merged.try_into().map_err(|e| e.to_string())?;
        restored.validate()?;
        *self = restored;
        Ok(())
    }

    /// 要启动的 shell:`config.shell` → `$SHELL` → `/bin/sh`。
    pub fn shell_command(&self) -> String {
        if !self.shell.is_empty() {
            return self.shell.clone();
        }
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
    }
}

const LUA_UI_PREFIX: &str = "-- Tay 界面设置：原脚本保留在函数内，界面修改集中在文件末尾；可直接编辑。\nlocal __tay_config = (function()\n-- TAY_BASE_BEGIN\n";
const LUA_UI_END: &str = "\n-- TAY_BASE_END\nend)()\n";

const LUA_FIELDS_BEGIN: &str = "-- TAY_UI_FIELDS_BEGIN\n";
const LUA_FIELDS_END: &str = "-- TAY_UI_FIELDS_END\n";

fn read_lua_source(path: &Path) -> Result<String, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok("return {}".into()),
        Err(e) => Err(e.to_string()),
    }
}

/// Old unmarked tails are user-owned in full. Never discard arbitrary Lua.
fn lua_user_tail(text: &str, config: &Config) -> Result<String, String> {
    let Some(body) = text.strip_prefix(LUA_UI_PREFIX) else {
        return Ok(String::new());
    };
    let (_, tail) = body.split_once(LUA_UI_END).ok_or("Lua 界面设置区损坏")?;
    let Some((before, rest)) = tail.split_once(LUA_FIELDS_BEGIN) else {
        return Ok(tail.to_string());
    };
    let (fields, after) = rest
        .split_once(LUA_FIELDS_END)
        .ok_or("Lua 界面字段区损坏，保留文件不覆盖")?;
    if after.contains(LUA_FIELDS_BEGIN) {
        return Err("重复的 Lua 界面字段区，保留文件不覆盖".into());
    }
    let values = toml::Value::try_from(config).map_err(|e| e.to_string())?;
    let mut allowed = std::collections::BTreeSet::new();
    for key in ["font", "colors"] {
        allowed.insert(format!("__tay_config.{key} = __tay_config.{key} or {{}}"));
        for (field, value) in values[key].as_table().ok_or("外观表无效")? {
            allowed.insert(format!(
                "__tay_config.{key}.{field} = {}",
                lua_value(value)?
            ));
        }
    }
    allowed.insert(format!("__tay_config.opacity = {}", config.opacity));
    if fields.lines().any(|line| !allowed.contains(line)) {
        return Err(
            "Lua 界面字段区含手写代码，请移到 TAY_UI_FIELDS_BEGIN 之前；保留文件不覆盖".into(),
        );
    }
    Ok(format!("{before}{after}"))
}

fn lua_base(text: &str) -> Result<&str, String> {
    if let Some(body) = text.strip_prefix(LUA_UI_PREFIX) {
        return body
            .split_once(LUA_UI_END)
            .map(|(base, _)| base)
            .ok_or_else(|| "Lua 界面设置区损坏，保留文件不覆盖".into());
    }
    if text.contains(LUA_UI_END) {
        return Err("Lua 配置包含保留的界面标记".into());
    }
    Ok(text)
}

fn lua_value(value: &toml::Value) -> Result<String, String> {
    match value {
        toml::Value::String(s) => {
            let mut quoted = String::from("\"");
            for c in s.chars() {
                match c {
                    '\\' => quoted.push_str("\\\\"),
                    '\"' => quoted.push_str("\\\""),
                    c if c.is_ascii_control() => quoted.push_str(&format!("\\{:03}", c as u32)),
                    c => quoted.push(c),
                }
            }
            quoted.push('"');
            Ok(quoted)
        }
        toml::Value::Integer(n) => Ok(n.to_string()),
        toml::Value::Float(n) => Ok(n.to_string()),
        toml::Value::Array(items) => Ok(format!(
            "{{ {} }}",
            items
                .iter()
                .map(lua_value)
                .collect::<Result<Vec<_>, _>>()?
                .join(", ")
        )),
        _ => Err("不支持的 Lua 外观值".into()),
    }
}

fn archive_legacy(path: &Path) -> Result<(), String> {
    let name = path.file_name().ok_or("旧配置路径无效")?.to_string_lossy();
    for n in 0..1000 {
        let suffix = if n == 0 {
            String::new()
        } else {
            format!(".{n}")
        };
        let backup = path.with_file_name(format!("{name}.migrated{suffix}.bak"));
        if !backup.exists() {
            return std::fs::rename(path, backup).map_err(|e| format!("归档旧配置失败: {e}"));
        }
    }
    Err("旧配置备份过多，无法归档".into())
}

fn write_lua_text(path: &Path, text: &str) -> Result<(), String> {
    write_lua_text_checked(path, text, None)
}

fn write_lua_text_checked(path: &Path, text: &str, expected: Option<&str>) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    Config::from_lua(text)?;
    let parent = path.parent().ok_or("配置路径无效")?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let tmp = parent.join(format!(
        ".config-{}-{}.tmp",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_nanos()
    ));
    let result = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        if let Ok(metadata) = std::fs::metadata(path) {
            file.set_permissions(metadata.permissions())?;
        }
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        if let Some(expected) = expected {
            let actual = read_lua_source(path).map_err(std::io::Error::other)?;
            if actual != expected {
                return Err(std::io::Error::other(
                    "Lua 配置在保存期间发生变化，请重试；未覆盖手动修改",
                ));
            }
        }
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result.map_err(|e| e.to_string())
}

/// Lua 值类型名(错误信息用)。
fn type_name(v: &mlua::Value) -> &'static str {
    match v {
        mlua::Value::Nil => "nil",
        mlua::Value::Boolean(_) => "布尔",
        mlua::Value::Integer(_) | mlua::Value::Number(_) => "数字",
        mlua::Value::String(_) => "字符串",
        mlua::Value::Table(_) => "表",
        _ => "其它",
    }
}

fn get_str(t: &mlua::Table, key: &str) -> Result<Option<String>, String> {
    match t.get::<mlua::Value>(key).map_err(|e| e.to_string())? {
        mlua::Value::Nil => Ok(None),
        mlua::Value::String(s) => s
            .to_str()
            .map(|s| Some(s.to_string()))
            .map_err(|e| e.to_string()),
        other => Err(format!("{key} 应为字符串,得到 {}", type_name(&other))),
    }
}

fn get_num(t: &mlua::Table, key: &str) -> Result<Option<f64>, String> {
    match t.get::<mlua::Value>(key).map_err(|e| e.to_string())? {
        mlua::Value::Nil => Ok(None),
        mlua::Value::Integer(n) => Ok(Some(n as f64)),
        mlua::Value::Number(n) => Ok(Some(n)),
        other => Err(format!("{key} 应为数字,得到 {}", type_name(&other))),
    }
}

fn get_uint(t: &mlua::Table, key: &str) -> Result<Option<u32>, String> {
    get_num(t, key)?
        .map(|v| {
            if v.is_finite() && v.fract() == 0.0 && (0.0..=u32::MAX as f64).contains(&v) {
                Ok(v as u32)
            } else {
                Err(format!("{key} 应为非负有限整数"))
            }
        })
        .transpose()
}

fn get_bool(t: &mlua::Table, key: &str) -> Result<Option<bool>, String> {
    match t.get::<mlua::Value>(key).map_err(|e| e.to_string())? {
        mlua::Value::Nil => Ok(None),
        mlua::Value::Boolean(b) => Ok(Some(b)),
        other => Err(format!("{key} 应为布尔,得到 {}", type_name(&other))),
    }
}

fn get_table(t: &mlua::Table, key: &str) -> Result<Option<mlua::Table>, String> {
    match t.get::<mlua::Value>(key).map_err(|e| e.to_string())? {
        mlua::Value::Nil => Ok(None),
        mlua::Value::Table(t) => Ok(Some(t)),
        other => Err(format!("{key} 应为表,得到 {}", type_name(&other))),
    }
}

fn get_color(t: &mlua::Table, key: &str) -> Result<Option<Rgb>, String> {
    match get_str(t, key)? {
        None => Ok(None),
        Some(s) => parse_hex(&s)
            .map(Some)
            .ok_or_else(|| format!("{key} 必须是 \"#rrggbb\" 格式,得到 {s:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lua 模板 roundtrip:to_lua 写出的文件必然可重新加载出
    /// 同一配置(模板生成/迁移共用同一序列化)。
    #[test]
    fn desktop_preset_and_custom_bindings_are_valid() {
        let cfg = Config::from_lua(include_str!("../../../config/desktop.lua")).unwrap();
        assert_eq!(cfg.theme, "wezterm");
        assert_eq!(cfg.global_shortcut, "CTRL+grave");
        assert_eq!(cfg.keys.len(), Config::default().keys.len());
        assert_eq!(cfg.colors.background, Rgb(31, 31, 40));
        assert!(Config::from_lua("return { keys={{key='t',mods='CTRL',action='oops'}} }").is_err());
        assert!(Config::from_lua("return { keys={{key='t',mods='CTRL',action='new-tab'},{key='T',mods='CTRL',action='hide'}} }").is_err());
        assert_eq!(Config::from_lua(&cfg.to_lua()).unwrap(), cfg);
    }
    #[test]
    fn key_aliases_font_weight_and_wezterm_palette_are_validated() {
        let config = Config::from_lua("return {theme='wezterm', font={weight=500}, disable_default_key_bindings=true, keys={{key='LeftArrow',mods='CTRL',action='previous-tab'}, {key='F12',mods='NONE',action='hide'}}}").unwrap();
        assert_eq!(config.colors.background, Rgb(31, 31, 40));
        assert_eq!(config.colors.foreground, Rgb(205, 214, 244));
        assert_eq!(config.colors.ansi[1], Rgb(243, 139, 168));
        assert_eq!(config.font.weight, 500);
        assert_eq!(Config::from_lua(&config.to_lua()).unwrap(), config);
        for weight in [0, 99, 901, 65536] {
            assert!(Config::from_lua(&format!("return {{font={{weight={weight}}}}}")).is_err());
        }
        assert!(Config::from_lua("return {keys={{key='Left',mods='CTRL',action='hide'}, {key='LeftArrow',mods='CTRL',action='none'}}}").is_err());
        let preset = Config::from_lua(include_str!("../../../config/desktop.lua")).unwrap();
        for binding in &Config::default().keys {
            assert!(
                preset.keys.contains(binding),
                "missing default binding: {binding:?}"
            );
        }
        assert_eq!(preset.binding_hint("new-tab"), "Ctrl+t");
        assert_eq!(config.binding_hint("new-tab"), "unbound");
    }

    #[test]
    fn partial_custom_keys_keep_defaults_and_can_disable_them() {
        let config =
            Config::from_lua("return {keys={{key='t',mods='CTRL|SHIFT',action='none'}}}").unwrap();
        assert!(
            config
                .keys
                .iter()
                .any(|b| b.key == "t" && b.mods == "CTRL" && b.action == "new-tab")
        );
        assert!(
            config
                .keys
                .iter()
                .any(|b| b.key == "t" && b.mods == "CTRL|SHIFT" && b.action == "none")
        );
        let empty =
            Config::from_lua("return {disable_default_key_bindings=true, keys={}}").unwrap();
        assert!(empty.keys.is_empty());
        assert_eq!(Config::from_lua(&config.to_lua()).unwrap(), config);
        assert_eq!(Config::from_lua(&empty.to_lua()).unwrap(), empty);
    }

    #[test]
    fn themes_and_indexed_palette_resolve_correctly() {
        let c = Colors::preset("midnight").unwrap();
        assert_eq!(c.indexed(1), c.ansi[1]);
        assert_eq!(c.indexed(16), Rgb(0, 0, 0));
        assert_eq!(c.indexed(21), Rgb(0, 0, 255));
        assert_eq!(c.indexed(231), Rgb(255, 255, 255));
        assert_eq!(c.indexed(232), Rgb(8, 8, 8));
        assert_eq!(c.indexed(255), Rgb(238, 238, 238));
        assert!(Config::from_lua("return { theme='unknown' }").is_err());
    }

    #[test]
    fn lua_integer_fields_reject_nan_negative_and_fractions() {
        for value in ["0/0", "-1", "1.5", "math.huge"] {
            for field in ["animation_ms", "height_percent"] {
                assert!(Config::from_lua(&format!("return {{ {field} = {value} }}")).is_err());
            }
        }
    }

    #[test]
    fn lua_has_no_side_effect_libraries_or_budget_escape() {
        let cfg = Config::from_lua("assert(os == nil and io == nil and package == nil and debug == nil and coroutine == nil and pcall == nil and load == nil); return {}").unwrap();
        assert_eq!(cfg, Config::default());
        assert!(
            Config::from_lua("while true do end")
                .unwrap_err()
                .contains("预算")
        );
        assert!(Config::from_lua("return {shell=string.rep('x', 16000000)}").is_err());
    }

    #[test]
    fn lua_ui_save_preserves_logic_comments_and_sparse_fields() {
        let dir = std::env::temp_dir().join(format!("tay-lua-save-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.lua");
        let source = "-- keep my logic\nlocal n=13; return {theme='midnight', font={size=n}, shell='/bin/sh', padding=7}";
        std::fs::write(&path, source).unwrap();
        let mut changed = Config::from_lua(source).unwrap();
        changed.font.size = 18.0;
        changed.save_appearance_at(&path).unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        assert_eq!(Config::from_lua(&saved).unwrap(), changed);
        assert_eq!(lua_base(&saved).unwrap(), source);
        assert!(!saved.contains("__tay_config.colors"));
        assert!(!dir.join("appearance.toml").exists());
        assert_eq!(
            std::fs::read_to_string(dir.join("config.lua.before-ui.bak")).unwrap(),
            source
        );
        changed.font.size = 20.0;
        changed.save_appearance_at(&path).unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        assert_eq!(saved.matches(LUA_UI_PREFIX).count(), 1);
        let light = saved.replace("theme='midnight'", "theme='light'");
        let reloaded = Config::from_lua(&light).unwrap();
        assert_eq!(reloaded.colors, Colors::preset("light").unwrap());
        assert_eq!(reloaded.font.size, 20.0);
        changed.font.size = 13.0;
        changed.save_appearance_at(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
        std::fs::write(&path, "this is broken Lua").unwrap();
        assert!(changed.save_appearance_at(&path).is_err());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "this is broken Lua"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn legacy_appearance_migrates_once_and_lua_is_the_only_source() {
        let dir = std::env::temp_dir().join(format!("tay-lua-migrate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.lua"),
            "return {theme='wezterm',font={size=12},padding=7}",
        )
        .unwrap();
        std::fs::write(dir.join("appearance.toml"), "[font]\nsize=18.0\n").unwrap();
        let loaded = Config::try_load_at(&dir).unwrap();
        assert_eq!(loaded.font.size, 18.0);
        assert_eq!(loaded.padding, 7.0);
        assert!(!dir.join("appearance.toml").exists());
        assert!(dir.join("appearance.toml.migrated.bak").exists());
        assert_eq!(Config::try_load_at(&dir).unwrap(), loaded);
        std::fs::write(
            dir.join("config.lua"),
            "return {theme='light',font={size=14}}",
        )
        .unwrap();
        assert_eq!(Config::try_load_at(&dir).unwrap().font.size, 14.0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn lua_saved_values_escape_strings_and_keep_existing_legacy_backups() {
        let dir = std::env::temp_dir().join(format!("tay-lua-escape-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), "opacity = 0.8\n").unwrap();
        let mut config = Config::try_load_at(&dir).unwrap();
        assert_eq!(config.opacity, 0.8);
        assert!(!dir.join("config.toml").exists());
        assert!(dir.join("config.toml.migrated.bak").exists());
        config.font.family = "quoted\" slash\\ newline\n中文".into();
        config.colors.ansi[1] = Rgb(1, 2, 3);
        config.save_appearance_at(&dir.join("config.lua")).unwrap();
        assert_eq!(Config::try_load_at(&dir).unwrap(), config);
        std::fs::write(dir.join("appearance.toml.migrated.bak"), "keep this backup").unwrap();
        std::fs::write(dir.join("appearance.toml"), "opacity=0.5").unwrap();
        assert_eq!(Config::try_load_at(&dir).unwrap().opacity, 0.5);
        assert_eq!(
            std::fs::read_to_string(dir.join("appearance.toml.migrated.bak")).unwrap(),
            "keep this backup"
        );
        assert!(dir.join("appearance.toml.migrated.1.bak").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn ui_delta_keeps_a_newer_manually_edited_theme() {
        let dir = std::env::temp_dir().join(format!("tay-lua-delta-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.lua");
        let previous = Config::from_lua("return {theme='midnight'}").unwrap();
        let mut changed = previous.clone();
        changed.font.size = 16.0;
        std::fs::write(
            &path,
            "-- newer manual edit\nreturn {theme='light',padding=9}",
        )
        .unwrap();
        changed.save_appearance_delta_at(&previous, &path).unwrap();
        let loaded = Config::try_load_at(&dir).unwrap();
        assert_eq!(loaded.theme, "light");
        assert_eq!(loaded.colors, Colors::preset("light").unwrap());
        assert_eq!(loaded.font.size, 16.0);
        assert_eq!(loaded.padding, 9.0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn manual_tail_survives_legacy_and_marked_ui_saves() {
        for legacy in [false, true] {
            let dir =
                std::env::temp_dir().join(format!("tay-lua-tail-{}-{legacy}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("config.lua");
            let source = "return {shell='/bin/bash',font={size=12}}";
            let text = if legacy {
                format!(
                    "{LUA_UI_PREFIX}{source}{LUA_UI_END}__tay_config.font.size = 13\nreturn __tay_config\n"
                )
            } else {
                std::fs::write(&path, source).unwrap();
                let mut c = Config::from_lua(source).unwrap();
                c.font.size = 13.0;
                c.save_appearance_at(&path).unwrap();
                std::fs::read_to_string(&path).unwrap()
            };
            let manual = "-- my manual settings\n__tay_config.shell = '/bin/fish'\n__tay_config.padding = 9\n";
            let text = text.replace(
                "return __tay_config",
                &format!("{manual}return __tay_config"),
            );
            std::fs::write(&path, &text).unwrap();
            for size in [14.0, 15.0] {
                let old = Config::try_load_at(&dir).unwrap();
                let mut new = old.clone();
                new.font.size = size;
                new.save_appearance_delta_at(&old, &path).unwrap();
                let loaded = Config::try_load_at(&dir).unwrap();
                assert_eq!(loaded.shell, "/bin/fish");
                assert_eq!(loaded.padding, 9.0);
                assert_eq!(loaded.font.size, size);
                let saved = std::fs::read_to_string(&path).unwrap();
                assert!(saved.contains(manual));
                assert_eq!(saved.matches(LUA_FIELDS_BEGIN).count(), 1);
            }
            // Unknown user code inside the owned block is protected, not deleted.
            let saved = std::fs::read_to_string(&path).unwrap().replace(
                LUA_FIELDS_END,
                &format!("__tay_config.animation_ms = 88\n{LUA_FIELDS_END}"),
            );
            std::fs::write(&path, &saved).unwrap();
            let mut c = Config::from_lua(&saved).unwrap();
            c.font.size = 16.0;
            assert!(c.save_appearance_at(&path).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), saved);
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn legacy_failures_keep_valid_lua_and_do_not_remove_pending_migration() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("tay-lua-readonly-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.lua"),
            "return {shell='/bin/fish',theme='wezterm'}",
        )
        .unwrap();
        std::fs::write(dir.join("config.toml"), "unused old config").unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        let loaded = Config::try_load_at(&dir);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(loaded.unwrap().shell, "/bin/fish");
        std::fs::write(dir.join("appearance.toml"), "invalid TOML [").unwrap();
        assert_eq!(Config::try_load_at(&dir).unwrap().theme, "wezterm");
        assert!(dir.join("appearance.toml").exists());
        std::fs::write(dir.join("appearance.toml"), "[font]\nsize=18.0\n").unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        let loaded = Config::try_load_at(&dir);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let loaded = loaded.unwrap();
        assert_eq!(loaded.shell, "/bin/fish");
        assert_eq!(loaded.font.size, 18.0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stale_save_is_rejected_without_overwriting_manual_edits() {
        let dir = std::env::temp_dir().join(format!("tay-lua-conflict-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.lua");
        let original = "return {font={size=12}}";
        let manual = "return {font={size=20},shell='/bin/fish'}";
        std::fs::write(&path, manual).unwrap();
        assert!(write_lua_text_checked(&path, "return {font={size=13}}", Some(original)).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), manual);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn lua_template_roundtrips() {
        let mut cfg = Config::default();
        cfg.font.family = "JetBrainsMono Nerd Font".into();
        cfg.font.size = 10.0;
        cfg.shell = "/bin/bash".into();
        cfg.height_percent = 55;
        cfg.padding = 6.0;
        cfg.opacity = 0.95;
        cfg.colors.cursor = Rgb::new(0x12, 0xab, 0xcd);
        let text = cfg.to_lua();
        assert!(text.contains("return {"));
        let back = Config::from_lua(&text).expect("模板必然可重载");
        assert_eq!(back, cfg);
    }

    /// 部分字段:缺省用默认值。
    #[test]
    fn lua_partial_fields_use_defaults() {
        let cfg =
            Config::from_lua("return { height_percent = 60, padding = 7 }").expect("部分字段合法");
        assert_eq!(cfg.height_percent, 60);
        assert_eq!(cfg.padding, 7.0);
        assert_eq!(cfg.font, Font::default());
        assert_eq!(cfg.colors, Colors::default());
    }

    /// 类型错误/越界/非表返回/语法错:整体拒绝(热重载沿用旧配置)。
    #[test]
    fn lua_errors_are_rejected() {
        assert!(Config::from_lua("return 5").is_err(), "非表");
        assert!(
            Config::from_lua("return { height_percent = \"50\" }").is_err(),
            "类型错"
        );
        assert!(
            Config::from_lua("return { opacity = 2.0 }").is_err(),
            "越界"
        );
        assert!(
            Config::from_lua("return { font = { size = 0 } }").is_err(),
            "字号越界"
        );
        assert!(
            Config::from_lua("return { colors = { background = \"red\" } }").is_err(),
            "颜色格式"
        );
        assert!(Config::from_lua("this is not lua (").is_err(), "语法错");
    }

    /// 沙箱:os/io/debug/package 不可用(配置不应有副作用)。
    #[test]
    fn lua_sandbox_blocks_os_and_io() {
        assert!(Config::from_lua("os.execute(\"touch /tmp/pwned\")").is_err());
        assert!(Config::from_lua("io.open(\"/etc/passwd\")").is_err());
        assert!(
            Config::from_lua("return { shell = \"x\" }").is_ok(),
            "正常配置仍可用"
        );
    }

    /// Lua 数字语义:整数与浮点都可(Lua 5.4 区分两者)。
    #[test]
    fn lua_accepts_integer_and_float_numbers() {
        let cfg = Config::from_lua("return { opacity = 1, padding = 6.5 }").unwrap();
        assert_eq!(cfg.opacity, 1.0);
        assert_eq!(cfg.padding, 6.5);
    }

    #[test]
    fn appearance_serializes_to_reloadable_config() {
        let config = Config::default();
        let text = toml::to_string(&config).unwrap();
        assert!(text.contains("#0f1114"));
        let reloaded: Config = toml::from_str(&text).unwrap();
        assert_eq!(reloaded, config);
    }

    #[test]
    fn parse_hex_roundtrip() {
        assert_eq!(parse_hex("#1e2a3b"), Some(Rgb(0x1e, 0x2a, 0x3b)));
        assert_eq!(parse_hex("1e2a3b"), None);
        assert_eq!(parse_hex("#1e2a3"), None);
        assert_eq!(parse_hex("#gggggg"), None);
    }

    /// P1d 回归:多字节 UTF-8 颜色串不能 panic,必须安全拒绝。
    /// "#aéé" = 6 字节,旧实现的 &s[0..2] 会在字符边界处 panic。
    #[test]
    fn parse_hex_multibyte_is_rejected_not_panicked() {
        assert_eq!(parse_hex("#aéé"), None);
        assert_eq!(parse_hex("#ééé"), None);
        assert_eq!(parse_hex("#fféé"), None);
        // 汉字也是多字节:同样只许拒绝。
        assert_eq!(parse_hex("#颜颜色"), None);
    }

    #[test]
    fn range_validation_rejects_bad_values() {
        let cases: Vec<(&str, &str)> = vec![
            ("height_percent = 400", "height_percent"),
            ("height_percent = 0", "height_percent"),
            ("opacity = 1.5", "opacity"),
            ("opacity = nan", "opacity"),
            ("opacity = inf", "opacity"),
            ("animation_ms = 999999", "animation_ms"),
            ("padding = nan", "padding"),
            ("[font]\nsize = 0.5", "font.size"),
        ];
        for (text, field) in cases {
            let cfg: Result<Config, _> = toml::from_str(text);
            // 要么 TOML 解析失败,要么范围校验失败;错误信息都要点名字段。
            let err = match cfg {
                Ok(c) => c.validate().expect_err(&format!("{text} 应被拒绝")),
                Err(e) => e.to_string(),
            };
            assert!(err.contains(field), "{err:?} 应提到 {field}");
        }
        // 合法边界值全部通过。
        for text in [
            "height_percent = 1",
            "height_percent = 100",
            "opacity = 0.0",
            "opacity = 1.0",
            "animation_ms = 0",
            "animation_ms = 5000",
            "padding = 0.0",
            "padding = 100.0",
            "[font]\nsize = 1.0",
            "[font]\nsize = 200.0",
        ] {
            let cfg: Config = toml::from_str(text).unwrap();
            cfg.validate()
                .unwrap_or_else(|e| panic!("{text} 应通过: {e}"));
        }
    }

    #[test]
    fn partial_config_merges_defaults() {
        let cfg: Config = toml::from_str(
            r#"
            [font]
            size = 14.0
            "#,
        )
        .unwrap();
        assert_eq!(cfg.font.size, 14.0);
        assert_eq!(cfg.font.family, Font::default().family);
        assert_eq!(cfg.colors, Colors::default());
    }

    #[test]
    fn full_config() {
        let cfg: Config = toml::from_str(
            r##"
            shell = "/usr/bin/fish"
            padding = 2.0
            opacity = 0.92

            [font]
            family = "FiraCode Nerd Font"
            size = 13.5

            [colors]
            foreground = "#eeeeff"
            background = "#101018"
            cursor = "#ff8800"
            selection = "#334466"
            "##,
        )
        .unwrap();
        assert_eq!(cfg.font.family, "FiraCode Nerd Font");
        assert_eq!(cfg.colors.cursor, Rgb::new(0xff, 0x88, 0x00));
        assert_eq!(cfg.shell, "/usr/bin/fish");
        assert_eq!(cfg.opacity, 0.92);
    }
}
