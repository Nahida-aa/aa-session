//! codebuddy 登录态读取（`CodeBuddyExtension` 凭据目录 + 本地账号配置）。
//!
//! # 数据在哪（2026-10 实测）
//!
//! | 内容 | 位置 |
//! |---|---|
//! | 账号 + OAuth token | `~/.local/share/CodeBuddyExtension/Data/Public/auth/<app>.info` |
//! | 历史轮转凭据 | 同目录 `<app>.<ISO时间戳>.<pid>.<uuid>.info`（登录/刷新时轮转） |
//! | 服务端账号配置 | `~/.codebuddy/local_storage/entry_*.info`（含 `userId` 的条目） |
//!
//! 当前生效的凭据文件**文件名不带时间戳**（`Tencent-Cloud.coding-copilot.info`），
//! 带时间戳的都是历史副本。`codebuddy config list` 的
//! `sandbox.filesystem.denyRead` 里明确把这个目录列为禁区，说明它是敏感目录。
//!
//! # 脱敏
//!
//! token 一律只输出前缀 + 长度，见 [`mask_token`]。手机号中间四位打码。

use std::path::{Path, PathBuf};

/// 凭据目录（相对 `$HOME`）。
pub const AUTH_DIR: &[&str] = &[
    ".local",
    "share",
    "CodeBuddyExtension",
    "Data",
    "Public",
    "auth",
];

/// 凭据目录默认位置（家目录展开后）。
pub fn auth_dir() -> PathBuf {
    let home = aa_session_core::home_dir().unwrap_or_default();
    let mut p = home;
    for seg in AUTH_DIR {
        p.push(seg);
    }
    p
}

/// token 脱敏：留前 12 字符 + 总长度。
///
/// 前 12 字符足够让用户区分「这是 access 还是 refresh」以及是否为空，
/// 又不至于把可用的凭据泄露到终端/日志/issue 里。
///
/// `show_secrets = true` 时原样返回 —— 调用方（CLI `--show-secrets`）必须
/// 已经给过用户警告。
pub fn mask_token(token: &str, show_secrets: bool) -> String {
    if show_secrets {
        return token.to_string();
    }
    let chars: Vec<char> = token.chars().collect();
    if chars.is_empty() {
        return "(空)".to_string();
    }
    let head: String = chars.iter().take(12).collect();
    format!("{head}…（{} 字符）", chars.len())
}

/// 手机号脱敏：保留前 3 后 4。
pub fn mask_phone(phone: &str) -> String {
    let chars: Vec<char> = phone.chars().collect();
    if chars.len() < 7 {
        return phone.to_string();
    }
    let head: String = chars.iter().take(3).collect();
    let tail: String = chars.iter().skip(chars.len() - 4).collect();
    format!("{head}****{tail}")
}

/// 一个 app 的凭据文件集合。
pub struct AppCredentials {
    /// app 名（当前凭据文件的 stem，如 `Tencent-Cloud.coding-copilot`）。
    pub app: String,
    /// 当前生效的凭据文件（文件名不带时间戳）；没有则为空。
    pub current: Option<PathBuf>,
    /// 历史轮转副本。
    pub rotated: Vec<PathBuf>,
}

/// 目录下的凭据文件，按 app 分组。
///
/// 判定规则：`<app>.info` 是当前；`<app>.<其它>...info` 且能被某个当前文件
/// 的 `<app>` 前缀匹配上的是它的历史副本。匹配不上任何当前文件的历史文件
/// 归到 `(孤儿)` 组（该 app 的当前凭据已被删，只剩轮转副本）。
///
/// 分组放在这里而不是让调用方看文件名猜 —— 轮转文件名里含时间戳段，
/// 调用方从文件名切不出 app 名（实测会把 83 个副本切成 83 个"app"）。
pub fn scan(dir: &Path) -> std::io::Result<Vec<AppCredentials>> {
    let mut all_info: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(dir)?.flatten() {
        let path = entry.path();
        if path.extension().map(|e| e == "info").unwrap_or(false) && path.is_file() {
            all_info.push(path);
        }
    }
    all_info.sort();

    let stem_of = |p: &Path| -> String {
        p.file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default()
    };

    // 轮转文件的 stem 里有一段 ISO 时间戳（含 `T` 且够长）。
    let is_rotated_stem = |stem: &str| {
        stem.split('.')
            .any(|seg| seg.contains('T') && seg.len() >= 15)
    };

    // 第一遍：当前文件 = stem 里没有时间戳段的。
    let mut apps: Vec<AppCredentials> = Vec::new();
    for path in &all_info {
        let stem = stem_of(path);
        if !is_rotated_stem(&stem) {
            apps.push(AppCredentials {
                app: stem,
                current: Some(path.clone()),
                rotated: Vec::new(),
            });
        }
    }

    // 第二遍：轮转文件归属「stem 是其前缀 + `.`」的那个 app。
    for path in &all_info {
        let stem = stem_of(path);
        if !is_rotated_stem(&stem) {
            continue;
        }
        let owner = apps
            .iter_mut()
            .find(|a| stem.starts_with(&format!("{}.", a.app)));
        match owner {
            Some(a) => a.rotated.push(path.clone()),
            // 没有对应当前文件的历史副本，单独归一组，别丢。
            None => apps.push(AppCredentials {
                app: "(孤儿轮转)".to_string(),
                current: None,
                rotated: vec![path.clone()],
            }),
        }
    }

    // 同一 app 只会有一个当前文件，但孤儿可能多个，摊平成多个条目。
    let mut out: Vec<AppCredentials> = Vec::new();
    for a in apps {
        if a.app == "(孤儿轮转)" {
            for p in a.rotated {
                out.push(AppCredentials {
                    app: "(孤儿轮转)".to_string(),
                    current: None,
                    rotated: vec![p],
                });
            }
        } else {
            out.push(a);
        }
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_token_shows_prefix_and_length_only() {
        let tok = "eyJhbGciOiJSUzI1NiIsInR5cCIgOiAiSldUIiwi.verylongrest";
        let m = mask_token(tok, false);
        assert!(m.starts_with("eyJhbGciOi"));
        // 长度按字符数，不泄露全串
        assert!(m.ends_with(&format!("（{} 字符）", tok.chars().count())));
        assert!(!m.contains("verylongrest"));
    }

    #[test]
    fn mask_token_handles_multibyte_and_empty() {
        assert_eq!(mask_token("", false), "(空)");
        // 中文字符数而非字节数
        assert!(mask_token("你好世界啊", false).contains("（5 字符）"));
    }

    #[test]
    fn show_secrets_returns_the_raw_token() {
        let tok = "eyJhbGciOiJSUzI1NiIsInR5cCIgOiAiSldUIiwi.verylongrest";
        assert_eq!(mask_token(tok, true), tok);
        // 一个字符都不能少
        assert_eq!(mask_token("", true), "");
    }

    #[test]
    fn mask_phone_keeps_head_and_tail() {
        assert_eq!(mask_phone("17386163744"), "173****3744");
        // 太短不处理，避免把有效位也码掉
        assert_eq!(mask_phone("12345"), "12345");
    }

    #[test]
    fn rotated_file_names_are_detected() {
        // 时间戳段含 T 且长度 >= 15
        let rotated = "Tencent-Cloud.coding-copilot.2026-08-19T22-35-13-981Z.193240.1b288854";
        assert!(rotated.split('.').any(|s| s.contains('T') && s.len() >= 15));

        let current = "Tencent-Cloud.coding-copilot";
        assert!(!current.split('.').any(|s| s.contains('T') && s.len() >= 15));
    }
}
