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
            // 基线用**当前 colors 的推导**,而不是主题名预设:颜色本身
            // 也被 UI 编辑过时(colors.background 等),从主题名推导会
            // 把颜色驱动的 tab_bar 偏移误判为用户定制,切主题后留下
            // 深色栏配浅色标签的错配。
            let old_colors = config.colors.clone();
            config.theme = preset.theme;
            config.colors = preset.colors;
            // 不先整体替换 tab_bar:rederive 从当前(用户)栏取定制
            // 字段,再按新颜色推导其余。
            rederive_tab_bar(config, &old_colors);
        }
        Field::Background | Field::Foreground => {
            let color = Rgb::from_hex(text).ok_or("请输入 #RRGGBB 颜色")?;
            // 颜色编辑即时重推导栏配色(否则要等 mtime 重载,期间深色
            // 栏配浅色终端;保存/重载失败时错配永久存在)。
            let old_colors = config.colors.clone();
            if field == Field::Background {
                config.colors.background = color;
            } else {
                config.colors.foreground = color;
            }
            rederive_tab_bar(config, &old_colors);
        }
    }
    Ok(())
}

/// 从当前 colors 重推导 tab_bar,保留布局字段与用户定制(相对
/// **旧推导**的偏移)。主题切换与颜色编辑共用:基线来自颜色本身
/// 而非主题名,颜色驱动的偏移不会误判为定制。
fn rederive_tab_bar(config: &mut Config, old_colors: &tay_config::Colors) {
    let tab = config.tab_bar.clone();
    let old = tay_config::TabBar::from_colors(old_colors);
    config.tab_bar = tay_config::TabBar::from_colors(&config.colors);
    // 布局字段始终保留。
    config.tab_bar.visible = tab.visible;
    config.tab_bar.compact = tab.compact;
    config.tab_bar.tab_width = tab.tab_width;
    // 用户定制字段(≠ 旧推导)保留。
    macro_rules! keep_custom {
        ($($field:ident),*) => {$(
            if tab.$field != old.$field {
                config.tab_bar.$field = tab.$field;
            }
        )*};
    }
    keep_custom!(
        background,
        active_background,
        active_foreground,
        inactive_foreground,
        hover_background,
        accent,
        icon_foreground,
        icon_hover_foreground,
        danger_foreground,
        clock_background,
        clock_foreground
    );
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
    #[test]
    fn theme_switch_preserves_user_customized_tab_bar_fields() {
        let mut c = Config::from_lua("return {theme='midnight'}").unwrap();
        // 模拟用户手写的定制(Lua 尾部或旧配置):时钟色/强调色。
        c.tab_bar.clock_background = Rgb(1, 2, 3);
        c.tab_bar.accent = Rgb(4, 5, 6);
        c.tab_bar.compact = false;
        apply(&mut c, Field::Theme, "light").unwrap();
        assert_eq!(c.theme, "light");
        assert_eq!(c.colors, tay_config::Colors::preset("light").unwrap());
        // 用户定制字段跨主题保留,布局字段也保留。
        assert_eq!(c.tab_bar.clock_background, Rgb(1, 2, 3));
        assert_eq!(c.tab_bar.accent, Rgb(4, 5, 6));
        assert!(!c.tab_bar.compact);
        // 未定制字段跟随新主题推导。
        let light = Config::from_lua("return {theme='light'}").unwrap();
        assert_eq!(c.tab_bar.background, light.tab_bar.background);
        assert_eq!(c.tab_bar.danger_foreground, light.tab_bar.danger_foreground);
    }
    #[test]
    fn color_edits_rederive_the_bar_and_do_not_survive_theme_switches() {
        // 用户只编辑背景色:栏配色即时跟随新颜色推导。
        let mut c = Config::from_lua("return {theme='default'}").unwrap();
        apply(&mut c, Field::Background, "#000000").unwrap();
        assert_eq!(c.colors.background, Rgb(0, 0, 0));
        assert_eq!(c.tab_bar, tay_config::TabBar::from_colors(&c.colors));
        // 切主题:颜色驱动的栏值不视为用户定制,整体跟随 light ——
        // 否则深色栏配浅色标签的错配会被 keep_custom 保留下来。
        apply(&mut c, Field::Theme, "light").unwrap();
        let light = Config::from_lua("return {theme='light'}").unwrap();
        assert_eq!(c.tab_bar, light.tab_bar);
    }
}
