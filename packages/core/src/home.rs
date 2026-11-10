//! 跨平台的家目录与 SQLite URI 拼装。
//!
//! 平台差异**只允许**集中在这里：Unix 上的行为与以前逐字节一致，Windows
//! 上的差异（没有 `HOME`、`canonicalize()` 返回 verbatim 路径）在这里消化掉，
//! 不散落到各 source。

use std::path::{Path, PathBuf};

/// 当前用户的家目录。
///
/// - Unix：`$HOME` —— XDG 规范的全部默认值都以它为前提。
/// - Windows：`%USERPROFILE%`。`HOME` 只在 Git Bash / MSYS 之类环境里存在，
///   原生 PowerShell / cmd 里没有。
///
/// 两个都取不到时返回 `None`，**绝不**退回空路径：空路径 join 出的是
/// `.local/share/opencode` 这种相对路径，会把「不知道家目录在哪」伪装成
/// 「库不存在」，正是这次 Windows 上误报的根源。
pub fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    const KEYS: &[&str] = &["USERPROFILE", "HOME"];
    #[cfg(not(windows))]
    const KEYS: &[&str] = &["HOME"];

    KEYS.iter()
        .copied()
        .filter_map(std::env::var_os)
        .map(PathBuf::from)
        .find(|path| !path.as_os_str().is_empty())
}

/// 生成 SQLite 的 `file:` URI。
///
/// Unix 上就是 `file:<路径>`，与以前一致。
///
/// Windows 上 `canonicalize()` 返回的是 `\\?\C:\...` 形式：带 verbatim 前缀、
/// 带反斜杠。直接塞进 `file:` URI 会被 SQLite 判为非法路径，报
/// `unable to open database file`。所以 Windows 分支先剥掉 `\\?\`，再把反斜杠
/// 换成正斜杠——SQLite 只认 URI 风格的 `/`。
pub fn sqlite_file_uri(path: &Path, read_only: bool) -> String {
    #[cfg(windows)]
    let path_text = {
        let text = path.to_string_lossy();
        let text = text.strip_prefix(r"\\?\").unwrap_or(text.as_ref());
        text.replace('\\', "/")
    };
    #[cfg(not(windows))]
    let path_text = path.display().to_string();

    if read_only {
        format!("file:{path_text}?mode=ro")
    } else {
        format!("file:{path_text}")
    }
}

/// Windows 长路径前缀。
///
/// Win32 的 `CreateFileW` / `FindFirstFileW` 在不加 `\\?\` 前缀时受 `MAX_PATH`
/// （260 字符）限制。CodeBuddy IDE 的消息文件路径会超过这个长度（窗口 uuid +
/// 工作区引用 + 会话 id + 消息 id 层层嵌套），不加前缀会直接报「找不到文件」。
///
/// - 已经带 `\\?\` 的路径不重复添加；
/// - 只处理绝对路径（verbatim 前缀要求路径是绝对的）；
/// - 前缀路径里**只能有反斜杠**，正斜杠要换掉；
/// - 非 Windows 原样返回，Unix 没有这个限制。
pub fn long_path(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let text = path.to_string_lossy();
        if text.starts_with(r"\\?\") || text.len() <= 260 || !path.is_absolute() {
            return path.to_path_buf();
        }
        PathBuf::from(format!(r"\\?\{}", text.replace('/', "\\")))
    }
    #[cfg(not(windows))]
    {
        path.to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn uri_has_no_windows_artifacts() {
        let uri = sqlite_file_uri(
            Path::new(r"\\?\C:\Users\aa\.local\share\opencode\opencode.db"),
            false,
        );
        assert!(!uri.contains('\\'), "URI 里不应出现反斜杠：{uri}");
        assert!(
            !uri.contains(r"\\?\"),
            "URI 里不应出现 verbatim 前缀：{uri}"
        );
        assert!(uri.starts_with("file:"), "{uri}");
    }

    #[cfg(windows)]
    #[test]
    fn long_path_adds_verbatim_prefix_only_when_needed() {
        let short = Path::new(r"C:\Users\aa\data");
        assert_eq!(long_path(short), short);

        let long = format!(r"C:\Users\aa\{}\file.json", "x".repeat(300));
        let prefixed = long_path(Path::new(&long));
        assert!(
            prefixed.to_string_lossy().starts_with(r"\\?\"),
            "{}",
            prefixed.display()
        );

        // 已经带前缀的不重复添加
        let already = Path::new(r"\\?\C:\already\long");
        assert_eq!(long_path(already), already);
    }

    #[test]
    fn long_path_is_identity_on_unix() {
        #[cfg(not(windows))]
        {
            let p = Path::new("/home/aa/data/session.json");
            assert_eq!(long_path(p), p);
        }
    }

    #[test]
    fn read_only_uri_keeps_mode_ro() {
        let uri = sqlite_file_uri(
            Path::new("/home/aa/.local/share/opencode/opencode.db"),
            true,
        );
        assert!(uri.ends_with("?mode=ro"), "{uri}");
    }
}
