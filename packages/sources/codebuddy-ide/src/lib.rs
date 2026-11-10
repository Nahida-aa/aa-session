//! CodeBuddy **IDE** 会话数据源（只读，直接扫磁盘上的 JSON 树）。
//!
//! 与 CLI 版 codebuddy（`~/.codebuddy/projects/**/*.jsonl`）完全独立：IDE 存储
//! 不是事件流，而是三层 JSON 文件。
//!
//! # 数据在哪（2026-11 实测，Windows）
//!
//! ```text
//! %LOCALAPPDATA%\CodeBuddyExtension\Data\<窗口 uuid>\CodeBuddyIDE\<工作区引用>\
//!     history\<md5(工作区路径)>\index.json          ← 该工作区的会话清单
//!     history\<md5>\<会话 id>\index.json             ← 会话的消息 id + 请求用量
//!     history\<md5>\<会话 id>\messages\<消息 id>.json ← 单条消息
//! ```
//!
//! - `index.json`（工作区级）：`{"conversations":[{id,name,createdAt,lastMessageAt,
//!   chatMode,selectedModelId,hidden,hiddenAt,...}],"current":...}`
//! - `index.json`（会话级）：`{"messages":[{id,role,isComplete}],
//!   "requests":[{messages:[id],usage:{inputTokens,outputTokens,...}}]}`
//! - 消息文件：`{"role","message":"<JSON 字符串>","id","extra","createdAt"}`，
//!   其中 `message` 解析后是 Anthropic 风格的内容块数组（`text` / `reasoning` /
//!   `tool-call` / `tool-result` / `image`）。
//!
//! # 工作区路径反查
//!
//! 目录名是**工作区路径的 md5**（实测 `md5("h:\\rustlearning")` =
//! `528be74bf57cbc3312bdf9a732d89e04`，路径用反斜杠、大小写按原样小写）。
//! 反查不靠猜：同一台机器上
//! `%APPDATA%\<产品名>\User\globalStorage\tencent-cloud.coding-copilot\genie-history\`
//! 下用 **base64 编码的路径**做目录名（`aDovcnVzdGxlYXJuaW5n` → `h:/rustlearning`），
//! 把每个路径按 md5 索引即可。工作区引用本身通常也是同一套 base64 路径，
//! 两者互相印证；都解不出来时才显示「（未知）」。
//!
//! # 长路径
//!
//! 消息文件路径轻易超过 260 字符（窗口 uuid + 工作区引用 + 会话 id + 消息 id），
//! 所有 I/O 都经 [`aa_session_core::long_path`] 加 `\\?\` 前缀。

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use aa_session_core::{
    DirectoryMeta, EmptySessions, Error, ProjectMeta, ProjectSelector, Result, SessionFilter,
    SessionMeta, SessionSource, SortKey, Subagents, UNKNOWN_PROJECT_PATH,
};
use base64::Engine as _;
use md5::{Digest, Md5};
use serde_json::Value;

pub mod markdown;

pub const APP_NAME: &str = "codebuddy-ide";

/// CodeBuddy IDE 数据根目录（其下有 `<窗口 uuid>/CodeBuddyIDE/...`）。
///
/// - Windows：`%LOCALAPPDATA%\CodeBuddyExtension\Data`
/// - macOS：`~/Library/Application Support/CodeBuddyExtension/Data`
/// - Linux：`$XDG_DATA_HOME/CodeBuddyExtension/Data`（缺省 `~/.local/share`）
pub fn default_data_root() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        let local = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .or_else(|| aa_session_core::home_dir().map(|h| h.join("AppData").join("Local")))?;
        Some(local.join("CodeBuddyExtension").join("Data"))
    }

    #[cfg(not(windows))]
    {
        let home = aa_session_core::home_dir()?;
        #[cfg(target_os = "macos")]
        let base = home.join("Library").join("Application Support");
        #[cfg(all(not(target_os = "macos"), unix))]
        let base = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local").join("share"));
        Some(base.join("CodeBuddyExtension").join("Data"))
    }
}

/// CodeBuddy IDE 的只读数据源。
pub struct CodebuddyIdeSource {
    data_root: PathBuf,
    /// md5(工作区路径) → 路径。反查要做目录扫描 + 少量消息探测，缓存一份。
    workspace_paths: std::sync::OnceLock<HashMap<String, String>>,
}

impl CodebuddyIdeSource {
    /// 从默认位置打开。
    pub fn open_default() -> Result<Self> {
        let root = default_data_root().ok_or_else(|| {
            Error::InvalidArgument(
                "无法定位 CodeBuddy IDE 数据目录（LOCALAPPDATA / HOME 均未设置）".into(),
            )
        })?;
        Self::open_dir(&root)
    }

    /// 从数据根目录打开（如 `%LOCALAPPDATA%\CodeBuddyExtension\Data`）。
    pub fn open_dir(dir: &Path) -> Result<Self> {
        if !exists_dir(dir) {
            return Err(Error::Open {
                path: dir.display().to_string(),
                cause: "directory does not exist".into(),
            });
        }
        Ok(Self {
            data_root: dir.to_path_buf(),
            workspace_paths: std::sync::OnceLock::new(),
        })
    }
}

/// 一个 `history/<md5>` 目录（同一工作区在不同窗口下可能重复出现）。
struct HistoryDir {
    /// 工作区引用目录名（通常是 base64 路径，也可能是窗口 uuid）。
    ws_ref: String,
    /// 工作区路径的 md5。
    md5: String,
    dir: PathBuf,
}

/// 一条会话（= 一个 `<会话 id>` 目录）。
struct Conversation {
    id: String,
    md5: String,
    ws_ref: String,
    project_path: String,
    dir: PathBuf,
    /// 工作区级 `index.json` 里这一项的原始对象。
    entry: Value,
    /// 会话级 `index.json`。
    index: Value,
}

impl Conversation {
    fn has_messages(&self) -> bool {
        self.index["messages"]
            .as_array()
            .is_some_and(|messages| !messages.is_empty())
    }
}

impl CodebuddyIdeSource {
    /// 枚举所有 `<窗口>/CodeBuddyIDE/<引用>/history/<md5>`。
    fn list_history_dirs(&self) -> Vec<HistoryDir> {
        let mut out = Vec::new();
        for window in list_dirs(&self.data_root) {
            let ide = window.join("CodeBuddyIDE");
            if !exists_dir(&ide) {
                continue;
            }
            for workspace in list_dirs(&ide) {
                let Some(ws_ref) = file_name(&workspace) else {
                    continue;
                };
                // 这个目录是 IDE 的缓存，不是工作区。
                if ws_ref == "genie-cache" {
                    continue;
                }
                let history = workspace.join("history");
                if !exists_dir(&history) {
                    continue;
                }
                for md5_dir in list_dirs(&history) {
                    let Some(md5) = file_name(&md5_dir) else {
                        continue;
                    };
                    out.push(HistoryDir {
                        ws_ref: ws_ref.clone(),
                        md5,
                        dir: md5_dir,
                    });
                }
            }
        }
        out
    }

    /// md5(工作区路径) → 工作区路径（进程内只算一次）。
    fn workspace_paths(&self) -> HashMap<String, String> {
        self.workspace_paths
            .get_or_init(|| self.build_workspace_paths())
            .clone()
    }

    fn build_workspace_paths(&self) -> HashMap<String, String> {
        let history = self.list_history_dirs();
        let mut map = HashMap::new();

        // 1. genie-history 的目录名是 base64 路径。
        for dir in genie_history_dirs() {
            for entry in list_dirs(&dir) {
                if let Some(name) = file_name(&entry)
                    && let Some(path) = decode_ref(&name)
                {
                    insert_path(&mut map, &path);
                }
            }
        }

        // 2. 工作区引用目录名本身。
        for h in &history {
            if let Some(path) = decode_ref(&h.ws_ref) {
                insert_path(&mut map, &path);
            }
        }

        // 3. 前两步覆盖不到的（多数窗口引用是 uuid），从会话里的
        //    `<user_info> Workspace Folder:` 兜底——每个工作区只读一条消息。
        for h in &history {
            if map.contains_key(&h.md5) {
                continue;
            }
            if let Some(path) = self.probe_workspace_path(&h.dir) {
                insert_path(&mut map, &path);
            }
        }

        map
    }

    /// 从某个 `history/<md5>` 目录的第一条用户消息里读 `Workspace Folder:`。
    ///
    /// 只读前 3 个会话、每个会话前 2 条用户消息；不适配成本失控。
    fn probe_workspace_path(&self, md5_dir: &Path) -> Option<String> {
        let index = read_json(&md5_dir.join("index.json"))?;
        for entry in index["conversations"]
            .as_array()
            .into_iter()
            .flatten()
            .take(3)
        {
            let Some(id) = entry["id"].as_str() else {
                continue;
            };
            let Some(conv_index) = read_json(&md5_dir.join(id).join("index.json")) else {
                continue;
            };
            let user_ids: Vec<&str> = conv_index["messages"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|m| m["role"].as_str() == Some("user"))
                .filter_map(|m| m["id"].as_str())
                .take(2)
                .collect();

            for message_id in user_ids {
                let Some(message) = read_json(
                    &md5_dir
                        .join(id)
                        .join("messages")
                        .join(format!("{message_id}.json")),
                ) else {
                    continue;
                };
                let content = markdown::inner_content(&message).unwrap_or_default();
                for block in content {
                    if block["type"].as_str() == Some("text")
                        && let Some(path) =
                            workspace_folder_from_text(block["text"].as_str().unwrap_or_default())
                    {
                        return Some(path);
                    }
                }
            }
        }
        None
    }

    fn list_conversations(&self) -> Vec<Conversation> {
        let history = self.list_history_dirs();
        let paths = self.workspace_paths();

        let mut out = Vec::new();
        for h in &history {
            let Some(index) = read_json(&h.dir.join("index.json")) else {
                continue;
            };
            let project_path = paths
                .get(&h.md5)
                .cloned()
                .unwrap_or_else(|| UNKNOWN_PROJECT_PATH.to_string());

            for entry in index["conversations"].as_array().into_iter().flatten() {
                let Some(id) = entry["id"].as_str() else {
                    continue;
                };
                let dir = h.dir.join(id);
                let conversation_index = read_json(&dir.join("index.json")).unwrap_or(Value::Null);
                out.push(Conversation {
                    id: id.to_string(),
                    md5: h.md5.clone(),
                    ws_ref: h.ws_ref.clone(),
                    project_path: project_path.clone(),
                    dir,
                    entry: entry.clone(),
                    index: conversation_index,
                });
            }
        }
        out
    }

    fn meta_of(&self, conv: &Conversation) -> SessionMeta {
        let entry = &conv.entry;

        let name = entry["name"].as_str().unwrap_or("").trim();
        let title = if name.is_empty() {
            format!("CodeBuddy IDE {}", conv.id)
        } else {
            name.to_string()
        };

        let time_created = entry["createdAt"]
            .as_str()
            .and_then(parse_timestamp_ms)
            .unwrap_or(0);
        let time_updated = entry["lastMessageAt"]
            .as_str()
            .and_then(parse_timestamp_ms)
            .unwrap_or(time_created);

        // 用量挂在「请求」上，一个请求可覆盖多条消息；按请求累加。
        let mut tokens_input = 0i64;
        let mut tokens_output = 0i64;
        for request in conv.index["requests"].as_array().into_iter().flatten() {
            let usage = &request["usage"];
            tokens_input += usage["inputTokens"].as_i64().unwrap_or(0);
            tokens_output += usage["outputTokens"].as_i64().unwrap_or(0);
        }

        let archived_at = if entry["hidden"].as_bool() == Some(true) {
            entry["hiddenAt"]
                .as_str()
                .and_then(parse_timestamp_ms)
                .or(Some(time_updated))
        } else {
            None
        };

        SessionMeta {
            app: APP_NAME.to_string(),
            id: conv.id.clone(),
            project_id: conv.md5.clone(),
            project_path: conv.project_path.clone(),
            parent_id: None,
            title,
            directory: if conv.project_path == UNKNOWN_PROJECT_PATH {
                String::new()
            } else {
                conv.project_path.clone()
            },
            agent: agent_of(entry),
            model: entry["selectedModelId"].as_str().map(str::to_string),
            time_created,
            time_updated,
            tokens_input,
            tokens_output,
            tokens_reasoning: 0,
            cost: None,
            files_changed: 0,
            archived_at,
        }
    }
}

impl SessionSource for CodebuddyIdeSource {
    fn app(&self) -> &'static str {
        APP_NAME
    }

    fn projects(&self) -> Result<Vec<ProjectMeta>> {
        let mut buckets: HashMap<String, ProjectMeta> = HashMap::new();

        for conv in self.list_conversations() {
            if !conv.has_messages() {
                continue;
            }
            let meta = self.meta_of(&conv);
            let bucket = buckets
                .entry(conv.md5.clone())
                .or_insert_with(|| ProjectMeta {
                    app: APP_NAME.to_string(),
                    id: conv.md5.clone(),
                    path: conv.project_path.clone(),
                    vcs: None,
                    session_count: 0,
                    subagent_count: 0,
                    archived_count: 0,
                    time_active: None,
                });
            if meta.archived_at.is_some() {
                bucket.archived_count += 1;
                continue;
            }
            bucket.session_count += 1;
            bucket.time_active = Some(
                bucket
                    .time_active
                    .map_or(meta.time_updated, |t| t.max(meta.time_updated)),
            );
        }

        let mut out: Vec<ProjectMeta> = buckets
            .into_values()
            .filter(|p| p.session_count > 0)
            .collect();
        out.sort_by_key(|p| std::cmp::Reverse(p.time_active.unwrap_or(0)));
        Ok(out)
    }

    fn project_paths_like(&self, pattern: &str) -> Result<Vec<String>> {
        let needle = pattern.to_lowercase();
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for project in self.projects()? {
            if project.path.to_lowercase().contains(&needle) && seen.insert(project.path.clone()) {
                out.push(project.path);
            }
        }
        Ok(out)
    }

    fn directories(&self) -> Result<Vec<DirectoryMeta>> {
        let mut by_dir: HashMap<String, (i64, i64)> = HashMap::new();
        for conv in self.list_conversations() {
            if !conv.has_messages() || conv.project_path == UNKNOWN_PROJECT_PATH {
                continue;
            }
            let meta = self.meta_of(&conv);
            if meta.archived_at.is_some() {
                continue;
            }
            let entry = by_dir.entry(conv.project_path.clone()).or_insert((0, 0));
            entry.0 += 1;
            entry.1 = entry.1.max(meta.time_updated);
        }

        let mut out: Vec<DirectoryMeta> = by_dir
            .into_iter()
            .map(|(path, (count, updated))| DirectoryMeta {
                path,
                session_count: count,
                time_updated: updated,
                project_count: 1,
            })
            .collect();
        out.sort_by_key(|d| std::cmp::Reverse(d.time_updated));
        Ok(out)
    }

    fn sessions(&self, filter: &SessionFilter, sort: SortKey) -> Result<Vec<SessionMeta>> {
        let paths: Option<Vec<String>> = match &filter.project {
            ProjectSelector::All => None,
            ProjectSelector::Exact { path } => Some(vec![path.clone()]),
            ProjectSelector::Like { paths, .. } if paths.is_empty() => return Ok(Vec::new()),
            ProjectSelector::Like { paths, .. } => Some(paths.clone()),
        };

        let mut out = Vec::new();
        for conv in self.list_conversations() {
            let meta = self.meta_of(&conv);

            // 空壳：会话级 index.json 里一条消息都没有。
            match filter.empty {
                EmptySessions::Exclude if !conv.has_messages() => continue,
                EmptySessions::Only if conv.has_messages() => continue,
                _ => {}
            }

            match filter.archived {
                aa_session_core::Archived::Exclude if meta.archived_at.is_some() => continue,
                aa_session_core::Archived::Only if meta.archived_at.is_none() => continue,
                _ => {}
            }

            // IDE 里没有子 agent 会话，`--subagents only` 自然是空集。
            if filter.subagents == Subagents::Only {
                continue;
            }

            if let Some(paths) = &paths
                && !paths.iter().any(|p| path_matches(&meta.project_path, p))
            {
                continue;
            }
            if let Some(directory) = &filter.directory
                && !meta.directory.contains(directory)
            {
                continue;
            }
            if !filter.agents.is_empty() {
                match &meta.agent {
                    Some(agent) if filter.agents.iter().any(|a| a == agent) => {}
                    _ => continue,
                }
            }
            if let Some(title) = &filter.title_contains
                && !meta.title.to_lowercase().contains(&title.to_lowercase())
            {
                continue;
            }
            if let Some(after) = filter.updated_after
                && meta.time_updated < after
            {
                continue;
            }

            out.push(meta);
        }

        apply_sort(&mut out, sort);
        out.truncate(filter.limit);
        Ok(out)
    }

    fn session(&self, session_id: &str) -> Result<Option<SessionMeta>> {
        Ok(self
            .list_conversations()
            .iter()
            .find(|c| c.id == session_id)
            .map(|c| self.meta_of(c)))
    }

    /// 导出会话的全部原始数据：工作区级条目 + 会话级 index + 每条消息文件。
    ///
    /// 消息按会话级 `index.json` 的顺序排列，目录里多出来的文件追加在后面，
    /// 一字节都不丢。
    fn dump(&self, session_id: &str) -> Result<Value> {
        let conversations = self.list_conversations();
        let conv = conversations
            .iter()
            .find(|c| c.id == session_id)
            .ok_or_else(|| Error::NotFound(format!("session {session_id}")))?;

        let messages_dir = conv.dir.join("messages");
        let mut ids: Vec<String> = conv.index["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| m["id"].as_str().map(str::to_string))
            .collect();
        let mut seen: HashSet<String> = ids.iter().cloned().collect();
        for file in list_files(&messages_dir) {
            if file.extension().is_some_and(|e| e == "json")
                && let Some(id) = file.file_stem().and_then(|s| s.to_str())
                && seen.insert(id.to_string())
            {
                ids.push(id.to_string());
            }
        }

        let messages: Vec<Value> = ids
            .iter()
            .filter_map(|id| read_json(&messages_dir.join(format!("{id}.json"))))
            .collect();

        Ok(serde_json::json!({
            "app": APP_NAME,
            "workspace": {
                "md5": conv.md5,
                "ref": conv.ws_ref,
                "path": conv.project_path,
            },
            "conversation": conv.index,
            "messages": messages,
        }))
    }

    fn markdown(&self, session_id: &str) -> Result<String> {
        let dump = self.dump(session_id)?;
        let messages = markdown::from_dump(&dump);
        let meta = self
            .session(session_id)?
            .ok_or_else(|| Error::NotFound(format!("session {session_id}")))?;
        Ok(aa_session_core::markdown::render(&meta, &messages))
    }

    fn auth(&self, show_secrets: bool) -> Result<Value> {
        Ok(self.read_auth(show_secrets))
    }
}

impl CodebuddyIdeSource {
    /// 登录态与 CLI 版 codebuddy 同源：`<数据根>/Public/auth/<app>.info`。
    fn read_auth(&self, show_secrets: bool) -> Value {
        let dir = self.data_root.join("Public").join("auth");
        let mut credentials = Vec::new();
        let mut rotated = 0usize;

        for file in list_files(&dir) {
            if !file.extension().is_some_and(|e| e == "info") {
                continue;
            }
            let Some(stem) = file.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            // 文件名不带时间戳的是当前凭据；带点的都是轮转副本。
            if stem.contains('.') {
                rotated += 1;
                continue;
            }
            let Some(raw) = read_json(&file) else {
                continue;
            };
            credentials.push(credential_summary(stem, &raw, show_secrets));
        }

        let logged_in = !credentials.is_empty();
        serde_json::json!({
            "app": APP_NAME,
            "status": if logged_in { "logged_in" } else { "not_logged_in" },
            "credential_dir": dir.display().to_string(),
            "credential_count": credentials.len(),
            "credentials": credentials,
            "rotated_file_count": rotated,
        })
    }
}

fn agent_of(entry: &Value) -> Option<String> {
    entry["chatMode"]
        .as_str()
        .or_else(|| entry["type"].as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// `mask_token`：留前 12 字符 + 总长度，避免凭据进入终端 / issue。
fn mask_token(token: &str, show_secrets: bool) -> String {
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

fn mask_phone(phone: &str) -> String {
    let chars: Vec<char> = phone.chars().collect();
    if chars.len() < 7 {
        return phone.to_string();
    }
    let head: String = chars.iter().take(3).collect();
    let tail: String = chars.iter().skip(chars.len() - 4).collect();
    format!("{head}****{tail}")
}

fn credential_summary(app: &str, raw: &Value, show_secrets: bool) -> Value {
    let mut out = serde_json::json!({ "app": app });

    if let Some(account) = raw.get("account") {
        let mut acc = serde_json::json!({ "uid": account["uid"] });
        // 昵称在部分版本里是 `{"$wbEncrypted":…}` 对象，解不出来就别把密文
        // 当昵称输出。
        if let Some(nickname) = account["nickname"].as_str() {
            acc["nickname"] = serde_json::json!(nickname);
        }
        if let Some(phone) = account["phoneNumber"].as_str() {
            acc["phoneNumberShown"] = serde_json::json!(if show_secrets {
                phone.to_string()
            } else {
                mask_phone(phone)
            });
        }
        out["account"] = acc;
    }

    if let Some(auth) = raw.get("auth") {
        let mut a = serde_json::json!({
            "expiresAt": auth["expiresAt"],
            "refreshExpiresAt": auth["refreshExpiresAt"],
        });
        for key in ["accessToken", "refreshToken"] {
            if let Some(token) = auth[key].as_str() {
                let target = if show_secrets {
                    key.to_string()
                } else {
                    format!("{key}Masked")
                };
                a[target.as_str()] = serde_json::json!(mask_token(token, show_secrets));
            }
        }
        if let Some(expires_at) = auth["expiresAt"].as_i64() {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            a["state"] = serde_json::json!(if expires_at <= now {
                "expired"
            } else if expires_at - now <= 7 * 86_400_000 {
                "expiring_soon"
            } else {
                "valid"
            });
        }
        out["auth"] = a;
    }

    out
}

// ---------------------------------------------------------------------------
// 平台 / 文件系统辅助
// ---------------------------------------------------------------------------

fn genie_history_dirs() -> Vec<PathBuf> {
    let mut bases: Vec<PathBuf> = Vec::new();
    #[cfg(windows)]
    {
        if let Some(dir) = std::env::var_os("APPDATA") {
            bases.push(PathBuf::from(dir));
        }
    }
    #[cfg(not(windows))]
    {
        if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
            bases.push(PathBuf::from(dir));
        }
        if let Some(home) = aa_session_core::home_dir() {
            bases.push(home.join(".config"));
            #[cfg(target_os = "macos")]
            bases.push(home.join("Library").join("Application Support"));
        }
    }

    let mut out = Vec::new();
    for base in bases {
        for product in list_dirs(&base) {
            let candidate = product
                .join("User")
                .join("globalStorage")
                .join("tencent-cloud.coding-copilot")
                .join("genie-history");
            if exists_dir(&candidate) {
                out.push(candidate);
            }
        }
    }
    out
}

/// 把 base64 目录名解成工作区路径（`_` 是 `=` 的占位符）。
fn decode_ref(name: &str) -> Option<String> {
    let mut text = name.replace('_', "=");
    if text.is_empty() {
        return None;
    }
    while !text.len().is_multiple_of(4) {
        text.push('=');
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(text)
        .ok()?;
    let path = String::from_utf8(bytes).ok()?;
    let looks_like_path = (path.starts_with('/')
        || (path.len() >= 3 && path.as_bytes()[1] == b':'))
        && path.contains('/')
        && !path.contains('\0');
    looks_like_path.then_some(path)
}

/// 一个工作区路径对应的 md5 候选，同时登记「原样」和「反斜杠」两种写法
/// —— Windows 用反斜杠，Unix 用正斜杠。
///
/// **大小写不能归一**：实测 `md5("g:\\LocalDub")` =
/// `09bd6a38d996c6f1d7c57db9b658b24d`，而 `md5("g:\\localdub")` 是另一个值。
fn workspace_md5_variants(path: &str) -> [String; 2] {
    [md5_hex(path), md5_hex(&path.replace('/', "\\"))]
}

fn md5_hex(text: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(text.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn insert_path(map: &mut HashMap<String, String>, path: &str) {
    for key in workspace_md5_variants(path) {
        map.entry(key).or_insert_with(|| path.to_string());
    }
}

fn read_json(path: &Path) -> Option<Value> {
    let text = fs::read_to_string(aa_session_core::long_path(path)).ok()?;
    match serde_json::from_str(&text) {
        Ok(value) => Some(value),
        Err(error) => {
            tracing::warn!(file = %path.display(), %error, "skipping malformed JSON");
            None
        }
    }
}

fn list_dirs(path: &Path) -> Vec<PathBuf> {
    fs::read_dir(aa_session_core::long_path(path))
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|p| exists_dir(p))
                .collect()
        })
        .unwrap_or_default()
}

fn list_files(path: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(aa_session_core::long_path(path))
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|p| fs::metadata(aa_session_core::long_path(p)).is_ok_and(|m| m.is_file()))
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files
}

fn exists_dir(path: &Path) -> bool {
    fs::metadata(aa_session_core::long_path(path)).is_ok_and(|m| m.is_dir())
}

fn file_name(path: &Path) -> Option<String> {
    path.file_name().map(|n| n.to_string_lossy().to_string())
}

/// 路径比较：Windows 上忽略大小写与斜杠方向，Unix 只统一斜杠。
fn path_matches(candidate: &str, expected: &str) -> bool {
    let candidate = normalize_path(candidate);
    let expected = normalize_path(expected);
    candidate == expected || candidate.starts_with(&format!("{expected}/"))
}

fn normalize_path(path: &str) -> String {
    #[cfg(windows)]
    {
        path.replace('\\', "/").to_lowercase()
    }
    #[cfg(not(windows))]
    {
        path.replace('\\', "/")
    }
}

/// 从注入的 `<user_info>` 文本里取 `Workspace Folder:`。
///
/// 实测格式：
/// `<user_info>\nOS Version: …\nWorkspace Folder: h:/grad-analyzer\n…</user_info>`
fn workspace_folder_from_text(text: &str) -> Option<String> {
    let marker = "Workspace Folder:";
    let rest = &text[text.find(marker)? + marker.len()..];
    let line = rest.lines().next()?.trim().trim_matches(['"', '`']);
    let looks_like_path = (line.starts_with('/')
        || (line.len() >= 3 && line.as_bytes()[1] == b':'))
        && line.contains('/')
        && !line.contains('\0');
    looks_like_path.then(|| line.to_string())
}

/// `2026-10-16T07:26:41.557Z` → Unix 毫秒。
fn parse_timestamp_ms(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    if bytes.len() < 20 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    let num = |from: usize, to: usize| -> Option<i64> { text.get(from..to)?.parse().ok() };

    let year = num(0, 4)?;
    let month = num(5, 7)?;
    let day = num(8, 10)?;
    let hour = num(11, 13)?;
    let minute = num(14, 16)?;
    let second = num(17, 19)?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    let millis: i64 = text
        .get(19..)
        .and_then(|rest| rest.strip_prefix('.'))
        .and_then(|rest| {
            let digits: String = rest
                .chars()
                .take_while(char::is_ascii_digit)
                .take(3)
                .collect();
            digits.parse().ok()
        })
        .unwrap_or(0);

    Some(
        epoch_days(year, month, day) * 86_400_000
            + (hour * 3600 + minute * 60 + second) * 1000
            + millis,
    )
}

/// 公历日期 → 1970-01-01 起的整天数（Howard Hinnant 的 `days_from_civil`）。
fn epoch_days(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn apply_sort(out: &mut [SessionMeta], sort: SortKey) {
    match sort {
        SortKey::UpdatedDesc => {
            out.sort_by(|a, b| b.time_updated.cmp(&a.time_updated).then(a.id.cmp(&b.id)))
        }
        SortKey::CreatedDesc => {
            out.sort_by(|a, b| b.time_created.cmp(&a.time_created).then(a.id.cmp(&b.id)))
        }
        SortKey::Title => out.sort_by(|a, b| {
            a.title
                .to_lowercase()
                .cmp(&b.title.to_lowercase())
                .then(a.id.cmp(&b.id))
        }),
        SortKey::Tokens => out.sort_by(|a, b| {
            let ta = a.tokens_input + a.tokens_output;
            let tb = b.tokens_input + b.tokens_output;
            tb.cmp(&ta).then(a.id.cmp(&b.id))
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_padded_base64_workspace_refs() {
        assert_eq!(
            decode_ref("aDovcnVzdGxlYXJuaW5n").as_deref(),
            Some("h:/rustlearning")
        );
        assert_eq!(
            decode_ref("YzovVXNlcnMvMjg2NDMvQ29kZUJ1ZGR5").as_deref(),
            Some("c:/Users/28643/CodeBuddy")
        );
        assert_eq!(decode_ref("aTovQWdlbnQ=").as_deref(), Some("i:/Agent"));
    }

    #[test]
    fn decodes_underscore_padding_from_genie_history() {
        assert_eq!(
            decode_ref("aDovcHJpbWFyeWhhY2tlcg__").as_deref(),
            Some("h:/primaryhacker")
        );
    }

    #[test]
    fn rejects_non_paths() {
        // 窗口 uuid 不是 base64 路径（含 `-`，解码后也不是路径）。
        assert_eq!(decode_ref("2c7d94cc-d91d-47ca-bdf1-7be73d6653f6"), None);
        assert_eq!(decode_ref("genie-cache"), None);
        assert_eq!(decode_ref(""), None);
    }

    #[test]
    fn md5_matches_the_observed_directory_name() {
        // 实测：`history/528be74bf57cbc3312bdf9a732d89e04` 对应 h:\rustlearning。
        let [as_is, backslash] = workspace_md5_variants("h:/rustlearning");
        assert_eq!(as_is, "cf527e1b14402bc3e28924f4f2c4e6ec");
        assert_eq!(backslash, "528be74bf57cbc3312bdf9a732d89e04");

        // 大小写敏感：`LocalDub` 不能被折成 `localdub`。
        let [_, backslash] = workspace_md5_variants("g:/LocalDub");
        assert_eq!(backslash, "09bd6a38d996c6f1d7c57db9b658b24d");
    }

    #[test]
    fn extracts_workspace_folder_from_user_info() {
        let text = "<user_info>\nOS Version: Windows 11\nWorkspace Folder: h:/grad-analyzer\n\
                    WorkSpace: h:/grad-analyzer\n</user_info>\n\n帮我看看代码";
        assert_eq!(
            workspace_folder_from_text(text).as_deref(),
            Some("h:/grad-analyzer")
        );
        assert_eq!(workspace_folder_from_text("没有这个字段"), None);
        assert_eq!(
            workspace_folder_from_text("Workspace Folder: relative/path"),
            None
        );
    }

    #[test]
    fn timestamp_parses_iso_millis() {
        assert_eq!(
            parse_timestamp_ms("2026-10-16T07:26:41.557Z"),
            Some(1_792_135_601_557)
        );
        assert_eq!(parse_timestamp_ms("not a timestamp"), None);
    }

    fn no_limit_filter() -> SessionFilter {
        SessionFilter {
            limit: aa_session_core::NO_LIMIT,
            ..Default::default()
        }
    }

    /// 造一棵最小 IDE 目录树：
    /// `Data/<窗口>/CodeBuddyIDE/<引用>/history/<md5>/...`
    fn temp_source() -> (PathBuf, CodebuddyIdeSource) {
        let base = std::env::temp_dir().join(format!(
            "aa-session-codebuddy-ide-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let md5 = "528be74bf57cbc3312bdf9a732d89e04";
        let history = base
            .join("win-uuid")
            .join("CodeBuddyIDE")
            .join("aDovcnVzdGxlYXJuaW5n")
            .join("history")
            .join(md5);
        let conv = history.join("conv-1");
        fs::create_dir_all(conv.join("messages")).unwrap();

        fs::write(
            history.join("index.json"),
            r#"{"conversations":[
                {"id":"conv-1","name":"第一条会话","type":"craft","chatMode":"craft",
                 "createdAt":"2026-10-16T07:26:41.557Z",
                 "lastMessageAt":"2026-10-17T04:56:01.779Z",
                 "selectedModelId":"deepseek-v4.1-flash"},
                {"id":"conv-2","name":"隐藏的会话","hidden":true,
                 "hiddenAt":"2026-10-29T01:33:01.692Z",
                 "createdAt":"2026-10-16T07:26:41.557Z",
                 "lastMessageAt":"2026-10-17T04:56:01.779Z"}
            ]}"#,
        )
        .unwrap();

        fs::write(
            conv.join("index.json"),
            r#"{"messages":[
                {"id":"m1","type":"text","role":"user","isComplete":true},
                {"id":"m2","type":"text","role":"assistant","isComplete":false}
            ],"requests":[
                {"id":"r1","messages":["m1","m2"],"usage":{"inputTokens":100,"outputTokens":10}},
                {"id":"r2","messages":["m1","m2"],"usage":{"inputTokens":200,"outputTokens":20}}
            ]}"#,
        )
        .unwrap();

        fs::write(
            conv.join("messages").join("m1.json"),
            r#"{"role":"user","id":"m1","createdAt":"2026-10-16T07:26:41.557Z",
                "message":"{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"你好\"}]}"}"#,
        )
        .unwrap();
        fs::write(
            conv.join("messages").join("m2.json"),
            r#"{"role":"assistant","id":"m2","createdAt":"2026-10-16T07:26:45.000Z",
                "message":"{\"role\":\"assistant\",\"content\":[{\"type\":\"reasoning\",\"text\":\"想一想\"},{\"type\":\"tool-call\",\"toolCallId\":\"c1\",\"toolName\":\"read_file\",\"args\":{\"path\":\"a.rs\"}},{\"type\":\"text\",\"text\":\"看完了\"}]}","extra":"{\"modelId\":\"x\"}"}"#,
        )
        .unwrap();
        fs::write(
            conv.join("messages").join("m3.json"),
            r#"{"role":"tool","id":"m3","message":"{\"role\":\"tool\",\"content\":[{\"type\":\"tool-result\",\"toolCallId\":\"c1\",\"toolName\":\"read_file\",\"result\":{\"status\":\"success\",\"success\":true,\"result\":{\"data\":[{\"type\":\"text\",\"text\":\"文件内容\"}]}}}]}"}"#,
        )
        .unwrap();

        // 一个被隐藏（归档）的会话，用来验证默认口径。
        let archived = history.join("conv-2");
        fs::create_dir_all(archived.join("messages")).unwrap();
        fs::write(
            archived.join("index.json"),
            r#"{"messages":[{"id":"m4","type":"text","role":"user","isComplete":true}],
                "requests":[{"id":"r3","usage":{"inputTokens":50,"outputTokens":5}}]}"#,
        )
        .unwrap();
        fs::write(
            archived.join("messages").join("m4.json"),
            r#"{"role":"user","id":"m4","message":"{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"归档的\"}]}"}"#,
        )
        .unwrap();

        let src = CodebuddyIdeSource::open_dir(&base).unwrap();
        (base, src)
    }

    #[test]
    fn lists_session_with_resolved_workspace_path() {
        let (dir, src) = temp_source();
        let sessions = src
            .sessions(&no_limit_filter(), SortKey::UpdatedDesc)
            .unwrap();
        assert_eq!(sessions.len(), 1, "{sessions:?}");
        let s = &sessions[0];
        assert_eq!(s.id, "conv-1");
        assert_eq!(s.title, "第一条会话");
        assert_eq!(s.project_path, "h:/rustlearning");
        assert_eq!(s.directory, "h:/rustlearning");
        assert_eq!(s.agent.as_deref(), Some("craft"));
        assert_eq!(s.model.as_deref(), Some("deepseek-v4.1-flash"));
        // 只累加非隐藏会话的请求用量。
        assert_eq!(s.tokens_input, 300);
        assert_eq!(s.tokens_output, 30);
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn hidden_conversation_is_archived() {
        let (dir, src) = temp_source();
        // 默认口径排除隐藏会话。
        let default = src
            .sessions(&no_limit_filter(), SortKey::UpdatedDesc)
            .unwrap();
        assert_eq!(default.len(), 1);
        assert_eq!(default[0].id, "conv-1");

        let only = src
            .sessions(
                &SessionFilter {
                    archived: aa_session_core::Archived::Only,
                    limit: aa_session_core::NO_LIMIT,
                    ..Default::default()
                },
                SortKey::UpdatedDesc,
            )
            .unwrap();
        assert_eq!(only.len(), 1, "{only:?}");
        assert_eq!(only[0].id, "conv-2");
        assert_eq!(only[0].archived_at, Some(1_793_237_581_692));
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn projects_and_directories_group_by_workspace() {
        let (dir, src) = temp_source();
        let projects = src.projects().unwrap();
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].path, "h:/rustlearning");
        assert_eq!(projects[0].session_count, 1);

        let dirs = src.directories().unwrap();
        assert_eq!(dirs.len(), 1);
        assert_eq!(dirs[0].path, "h:/rustlearning");
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn dump_keeps_conversation_index_and_messages() {
        let (dir, src) = temp_source();
        let dump = src.dump("conv-1").unwrap();
        assert_eq!(dump["app"], "codebuddy-ide");
        assert_eq!(dump["workspace"]["path"], "h:/rustlearning");
        // 会话索引里有 2 条消息 id，但目录里多出一个 m3 —— 必须追加而不是丢掉。
        assert_eq!(dump["messages"].as_array().unwrap().len(), 3);
        assert_eq!(
            src.dump("nope").unwrap_err().to_string(),
            "not found: session nope"
        );
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn markdown_renders_tool_call_and_result() {
        let (dir, src) = temp_source();
        let md = src.markdown("conv-1").unwrap();
        assert!(md.contains("## User\n\n你好"), "{md}");
        assert!(md.contains("<thinking>\n想一想\n</thinking>"), "{md}");
        assert!(md.contains("**Tool Use**: read_file (ID: c1)"), "{md}");
        assert!(
            md.contains("**Tool Result**: read_file (ID: c1)\n\n文件内容"),
            "{md}"
        );
        fs::remove_dir_all(dir).ok();
    }
}
