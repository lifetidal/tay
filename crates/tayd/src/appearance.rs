//! Appearance edits are validated before touching the live renderer or Lua file.
use crate::chrome::Field;
use tay_config::{Config, Rgb};
pub(crate) fn apply(config: &mut Config, field: Field, text: &str) -> Result<(), String> {
    let number = |min: f32, max: f32| -> Result<f32, String> {
        let value: f32 = text.trim().parse().map_err(|_| "请输入有效数字")?;
        if value.is_finite() && (min..=max).contains(&value) {
            Ok(value)
        } else {
            Err(format!("数值范围：{min}–{max}"))
        }
    };
    let percent = || -> Result<f32, String> {
        if let Some(value) = text.trim().strip_suffix('%') {
            let value: f32 = value.trim().parse().map_err(|_| "请输入百分比，例如 80%")?;
            if value.is_finite() && (0.0..=100.0).contains(&value) {
                Ok(value / 100.0)
            } else {
                Err("百分比范围：0%–100%".into())
            }
        } else {
            number(0.0, 1.0)
        }
    };
    match field {
        Field::Font => {
            if text.trim().is_empty() {
                return Err("字体名称不能为空".into());
            }
            config.font.family = text.trim().into();
        }
        Field::Size => config.font.size = number(6.0, 48.0)?,
        Field::Opacity => config.opacity = percent()?,
        Field::ImageBrightness => config.background_image_brightness = percent()?,
        Field::ImageOpacity => config.background_image_opacity = percent()?,
        Field::Wallpaper => config.background_image = text.trim().into(),
        Field::Theme => {
            let theme = text.trim();
            if !["default", "midnight", "light", "wezterm"].contains(&theme) {
                return Err("主题：default / midnight / light / wezterm".into());
            }
            let preset = Config::from_lua(&format!("return {{theme='{theme}'}}"))?;
            config.theme = preset.theme;
            config.colors = preset.colors;
            let (visible, compact, width) = (
                config.tab_bar.visible,
                config.tab_bar.compact,
                config.tab_bar.tab_width,
            );
            config.tab_bar = preset.tab_bar;
            config.tab_bar.visible = visible;
            config.tab_bar.compact = compact;
            config.tab_bar.tab_width = width;
        }
        Field::Background | Field::Foreground => {
            let color = Rgb::from_hex(text).ok_or("请输入 #RRGGBB 颜色")?;
            if field == Field::Background {
                config.colors.background = color;
            } else {
                config.colors.foreground = color;
            }
        }
    }
    Ok(())
}
pub(crate) fn reset(config: &mut Config) {
    restore(config, &Config::default());
}
pub(crate) fn restore(config: &mut Config, source: &Config) {
    config.theme = source.theme.clone();
    config.colors = source.colors.clone();
    config.font = source.font.clone();
    config.tab_bar = source.tab_bar.clone();
    config.opacity = source.opacity;
    config.background_image = source.background_image.clone();
    config.background_image_opacity = source.background_image_opacity;
    config.background_image_brightness = source.background_image_brightness;
    config.background_blur = source.background_blur;
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn percentages_and_undo_preserve_nonappearance_settings() {
        let mut c = Config::default();
        let before = c.clone();
        apply(&mut c, Field::ImageBrightness, "45%").unwrap();
        assert_eq!(c.background_image_brightness, 0.45);
        assert!(apply(&mut c, Field::Opacity, "150%").is_err());
        c.shell = "/bin/fish".into();
        c.global_shortcut = "CTRL+grave".into();
        restore(&mut c, &before);
        assert_eq!(c.shell, "/bin/fish");
        assert_eq!(c.global_shortcut, "CTRL+grave");
    }
    #[test]
    fn invalid_edits_leave_config_unchanged_and_reset_preserves_shell() {
        let mut c = Config::default();
        let original = c.clone();
        assert!(apply(&mut c, Field::Opacity, "NaN").is_err());
        assert_eq!(c, original);
        assert!(apply(&mut c, Field::Theme, "';error('x')").is_err());
        apply(&mut c, Field::Theme, "light").unwrap();
        assert_eq!(c.theme, "light");
        c.shell = "/bin/fish".into();
        reset(&mut c);
        assert_eq!(c.shell, "/bin/fish");
        assert_eq!(c.colors, original.colors);
    }
}
