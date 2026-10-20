//! 会话持久化:优雅退出时把标签骨架(外壳命令、工作目录、标题)
//! 写入 XDG state 下的 session.toml;`tay resurrect` 拉起 daemon 时
//! 按骨架恢复标签。
//!
//! 只保存骨架,**不保存终端网格内容**(alacritty Term 不可序列化);
//! 滚动历史/图像/命令块的完整恢复留作后续(需要内核侧状态导出)。
//! 崩溃(非优雅退出)不会更新会话文件 —— 文件只在 quit 时写。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 会话文件单标签条目。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PersistTab {
    /// 该标签的 shell 命令(覆盖当前配置默认值)。
    pub shell: String,
    /// 退出时刻的工作目录(/proc/<pid>/cwd;子进程已退出 → None)。
    pub cwd: Option<String>,
    pub title: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct File {
    version: u32,
    active: usize,
    tabs: Vec<PersistTab>,
}

/// 会话文件路径(XDG state 目录;与配置目录分离 —— 状态不参与同步)。
pub fn path() -> PathBuf {
    dirs::state_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("tay")
        .join("session.toml")
}

/// 子进程工作目录(/proc/<pid>/cwd 实时指向;进程已退出 → None)。
pub fn cwd_of(pid: u32) -> Option<String> {
    std::fs::read_link(format!("/proc/{pid}/cwd"))
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

/// 原子写会话文件(临时文件 + rename);失败返回错误,由调用方
/// 记日志 —— quit 不得因保存失败而受阻。
pub fn save(tabs: &[PersistTab], active: usize) -> std::io::Result<()> {
    save_at(&path(), tabs, active)
}

fn save_at(path: &Path, tabs: &[PersistTab], active: usize) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = toml::to_string_pretty(&File {
        version: 1,
        active,
        tabs: tabs.to_vec(),
    })
    .map_err(std::io::Error::other)?;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(tmp, path)
}

/// 读会话文件;缺失/损坏/版本不符/空标签 → None(调用方回退到
/// 默认单标签启动)。
pub fn load() -> Option<(Vec<PersistTab>, usize)> {
    load_at(&path())
}

fn load_at(path: &Path) -> Option<(Vec<PersistTab>, usize)> {
    let text = std::fs::read_to_string(path).ok()?;
    let file: File = toml::from_str(&text).ok()?;
    if file.version != 1 || file.tabs.is_empty() {
        return None;
    }
    let active = file.active.min(file.tabs.len() - 1);
    Some((file.tabs, active))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("tay-persist-test-{name}-{}", std::process::id()))
    }

    /// 保存 → 读回:字段逐项一致,active 钳制到合法范围。
    #[test]
    fn roundtrip_preserves_tabs_and_clamps_active() {
        let path = tmp_path("roundtrip");
        let _ = std::fs::remove_file(&path);
        let tabs = vec![
            PersistTab {
                shell: "/bin/bash".into(),
                cwd: Some("/home/weiyi/proj".into()),
                title: "bash".into(),
            },
            PersistTab {
                shell: "fish".into(),
                cwd: None,
                title: "fish".into(),
            },
        ];
        save_at(&path, &tabs, 99).expect("save");
        let (got, active) = load_at(&path).expect("load");
        assert_eq!(got, tabs);
        assert_eq!(active, 1, "active 越界钳到最后");
        let _ = std::fs::remove_file(&path);
    }

    /// 损坏文件 / 空标签 → None(不 panic,不产出半截状态)。
    #[test]
    fn corrupt_or_empty_file_loads_none() {
        let path = tmp_path("corrupt");
        std::fs::write(&path, "version = 1\ntabs = [").unwrap();
        assert!(load_at(&path).is_none());
        std::fs::write(&path, "version = 1\nactive = 0\ntabs = []\n").unwrap();
        assert!(load_at(&path).is_none());
        let _ = std::fs::remove_file(&path);
    }

    /// 版本不符 → None(前向兼容保护)。
    #[test]
    fn unknown_version_loads_none() {
        let path = tmp_path("version");
        std::fs::write(
            &path,
            "version = 99\nactive = 0\ntabs = [{shell='/bin/sh'}]\n",
        )
        .unwrap();
        assert!(load_at(&path).is_none());
        let _ = std::fs::remove_file(&path);
    }
}
