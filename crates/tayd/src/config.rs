//! 配置状态(架构 §6):期望(磁盘)vs 实际运行配置。
//!
//! 热重载决策为纯函数(可控时钟/可测);**无效热重载保留有效状态**
//! (沿用最后一个成功版本),同一错误版本只告警一次,修好文件后
//! 自动恢复。
//!
//! 逐字段生效性:
//! - **即时生效**:font(字体/字号)、colors(调色板)、opacity(透明度)、animation_ms、
//!   hide_on_focus_out、padding(触发布局重算)、height_percent
//!   (触发 sizing 流程);
//! - **需重启**:shell(已启动)、
//!   output(已绑定)、start_hidden(启动时消费)。

use std::time::SystemTime;

use tay_config::Config;

/// 热重载尝试的结果。
#[derive(Debug, Clone, PartialEq)]
pub enum Reload {
    /// 解析成功,携带新配置(调用方替换运行配置并处理即时字段)。
    Ok(Box<Config>),
    /// 解析失败:沿用有效配置;`repeat` = 与上次失败同一版本
    /// (同一错误只告警一次);`error` = 解析错误详情(告警用)。
    Err { repeat: bool, error: String },
}

/// 热重载决策(纯函数)。
/// `mtime` = 磁盘版本标识;`failed_mtime` = 最近一次失败版本
/// (由本函数维护);`load` = 本次解析结果。
pub fn reload_decision(
    mtime: SystemTime,
    failed_mtime: &mut Option<SystemTime>,
    load: Result<Config, String>,
) -> Reload {
    match load {
        Ok(fresh) => {
            *failed_mtime = None;
            Reload::Ok(Box::new(fresh))
        }
        Err(e) => {
            let repeat = *failed_mtime == Some(mtime);
            *failed_mtime = Some(mtime);
            Reload::Err { repeat, error: e }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn mtime(t: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(t)
    }

    fn cfg() -> Config {
        Config::default()
    }

    /// 无效热重载保留有效状态:Err 不携带配置,调用方沿用旧值;
    /// 同一错误版本 repeat=true(只告警一次);修好(新 mtime)后恢复。
    #[test]
    fn invalid_reload_keeps_effective_config() {
        let mut failed = None;
        // 成功。
        assert_eq!(
            reload_decision(mtime(1), &mut failed, Ok(cfg())),
            Reload::Ok(Box::new(cfg()))
        );
        assert_eq!(failed, None);
        // 同一版本的坏文件:第一次告警、第二次静默。
        assert_eq!(
            reload_decision(mtime(2), &mut failed, Err("坏 TOML".into())),
            Reload::Err {
                repeat: false,
                error: "坏 TOML".into()
            }
        );
        assert_eq!(
            reload_decision(mtime(2), &mut failed, Err("坏 TOML".into())),
            Reload::Err {
                repeat: true,
                error: "坏 TOML".into()
            },
            "同一错误版本重复告警会被抑制"
        );
        // 修好(新 mtime)→ 恢复。
        assert_eq!(
            reload_decision(mtime(3), &mut failed, Ok(cfg())),
            Reload::Ok(Box::new(cfg()))
        );
        assert_eq!(failed, None);
    }
}
