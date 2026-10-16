//! Tay 配置:TOML 解析、默认值、XDG 路径。
//!
//! 零配置哲学:没有配置文件时全部字段有可用默认值;配置只覆盖
//! 用户显式写出的键(serde `#[serde(default)]` 语义)。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// RGB 颜色(0–255),TOML 里写作 `"#rrggbb"`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
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
    if s.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some(Rgb(r, g, b))
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct Font {
    /// 字体族名称,cosmic-text 的 fontdb 会做回退(CJK / Nerd Font)。
    pub family: String,
    /// 字号(pt)。
    pub size: f32,
}

impl Default for Font {
    fn default() -> Self {
        Self {
            family: "JetBrainsMono Nerd Font".into(),
            size: 12.0,
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
}

impl Default for Colors {
    fn default() -> Self {
        Self {
            foreground: Rgb::new(0xd4, 0xd4, 0xd4),
            background: Rgb::new(0x0f, 0x11, 0x14),
            cursor: Rgb::new(0x00, 0xa8, 0x84),
            selection: Rgb::new(0x39, 0x5e, 0x9c),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub font: Font,
    pub colors: Colors,
    /// 启动的 shell;空串 = 取 `$SHELL`(再兜底 `/bin/sh`)。
    pub shell: String,
    /// 单元格内边距(px,每条边),留白让 CJK/图标不贴框。
    pub padding: f32,
    /// 窗口不透明度 0.0–1.0,1.0 = 完全不透明(Wayland 模糊是增强项,见 DESIGN §6.4)。
    pub opacity: f32,
    /// 呼出时终端高度占输出高度的百分比。
    pub height_percent: u32,
    /// 呼出/收起动画时长(ms),0 = 无动画。
    pub animation_ms: u32,
    /// 失焦自动隐藏(Wayland 上即 keyboard leave;见 DESIGN §6.3)。
    pub hide_on_focus_out: bool,
    /// daemon 启动时隐藏(M1 常驻模型)。
    pub start_hidden: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            font: Font::default(),
            colors: Colors::default(),
            shell: String::new(),
            padding: 3.0,
            opacity: 1.0,
            height_percent: 45,
            animation_ms: 150,
            hide_on_focus_out: true,
            start_hidden: true,
        }
    }
}

impl Config {
    /// 配置文件路径:$XDG_CONFIG_HOME/tay/config.toml
    pub fn config_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("tay")
            .join("config.toml")
    }

    /// 读取配置:默认值 + 配置文件覆盖。文件缺失或损坏都退回默认值
    /// (损坏时报错日志但不阻止启动——呼出可用性优先)。
    pub fn load() -> Config {
        let path = Self::config_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => match toml::from_str::<Config>(&text) {
                Ok(cfg) => cfg,
                Err(e) => {
                    eprintln!("警告:配置文件 {} 解析失败,使用默认配置: {e}", path.display());
                    Config::default()
                }
            },
            Err(_) => Config::default(),
        }
    }

    /// 要启动的 shell:`config.shell` → `$SHELL` → `/bin/sh`。
    pub fn shell_command(&self) -> String {
        if !self.shell.is_empty() {
            return self.shell.clone();
        }
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hex_roundtrip() {
        assert_eq!(parse_hex("#1e2a3b"), Some(Rgb(0x1e, 0x2a, 0x3b)));
        assert_eq!(parse_hex("1e2a3b"), None);
        assert_eq!(parse_hex("#1e2a3"), None);
        assert_eq!(parse_hex("#gggggg"), None);
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
