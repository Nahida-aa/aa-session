//! Claude Code 会话数据源（只读，直接扫 JSONL 文件）。
//!
//! 会话存在 `~/.claude/projects/<编码路径>/<session-uuid>.jsonl`，每个文件一条
//! 会话的事件流。
//!
//! # 目录名编码
//!
//! `/home/aa/repos/env_ls/resume` → `-home-aa-repos-env-ls-resume`（`/` 换成 `-`，
//! 开头补一个 `-`）。路径段自身可以含 `-`（`env_ls`、`gpui-learn`），所以**不反向
//! 拆分**：项目路径一律取事件里的 `cwd` 字段，目录名只当项目 id。
//!
//! # 事件流
//!
//! 每行一个 JSON 对象，`type` 字段决定含义。实测一条 125 行的会话包含 13 种
//! `type`，其中只有 `user` / `assistant` 是对话内容，其余是运行时状态：
//!
//! | type | 用途 |
//! | --- | --- |
//! | `user` / `assistant` | 对话消息（标题、时间、token、成本） |
//! | `system` | 回合统计（`turn_duration` + `messageCount`） |
//! | `attachment` / `api-request*` / `mode` / `permission-mode` /
//!   `file-history-snapshot` / `last-prompt` / `atis-latch` | 运行时状态，原样留在 `dump()` 里 |
//!
//! 注意 `assistant` 事件**不是一条消息**：同一条 API 消息的每个内容块各占一行，
//! 靠 `message.id` 相同来标识。元数据统计不受影响，但 Markdown 渲染必须按
//! `message.id` 归并（见 `markdown` 模块）。

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use aa_session_core::{
    DirectoryMeta, Error, ProjectMeta, ProjectSelector, Result, SessionFilter, SessionMeta,
    SessionSource, SortKey,
};

pub mod markdown;

pub const APP_NAME: &str = "claude";
pub const DEFAULT_DATA_DIR: &str = ".claude";
pub const PROJECTS_DIR: &str = "projects";

/// Claude Code 的只读数据源（JSONL 文件，无数据库）。
pub struct ClaudeSource {
    root: PathBuf,
}

impl ClaudeSource {
    /// 从家目录展开后的默认位置打开。
    pub fn open_default() -> Result<Self> {
        let home = aa_session_core::home_dir().ok_or_else(|| {
            Error::InvalidArgument("无法定位用户家目录（HOME / USERPROFILE 均未设置）".into())
        })?;
        Self::open_dir(&home.join(DEFAULT_DATA_DIR))
    }

    /// 从数据目录打开（如 `~/.claude`）。
    pub fn open_dir(dir: &Path) -> Result<Self> {
        if !dir.exists() {
            return Err(Error::Open {
                path: dir.display().to_string(),
                cause: "directory does not exist".into(),
            });
        }
        Ok(Self {
            root: dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf()),
        })
    }

    fn projects_dir(&self) -> PathBuf {
        self.root.join(PROJECTS_DIR)
    }

    /// 遍历 `projects/` 下所有 `.jsonl` 会话文件，连同所属项目目录 id。
    fn list_session_files(&self) -> Result<Vec<(String, PathBuf)>> {
        let projects_dir = self.projects_dir();
        if !projects_dir.exists() {
            return Ok(Vec::new());
        }

        let mut files = Vec::new();
        let entries = fs::read_dir(&projects_dir).map_err(|e| Error::Open {
            path: projects_dir.display().to_string(),
            cause: e.to_string(),
        })?;

        for entry in entries.flatten() {
            let proj_dir = entry.path();
            if !proj_dir.is_dir() {
                continue;
            }
            let project_id = proj_dir
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            if let Ok(sessions) = fs::read_dir(&proj_dir) {
                for s in sessions.flatten() {
                    let p = s.path();
                    if p.extension().map(|e| e == "jsonl").unwrap_or(false) && !p.is_dir() {
                        files.push((project_id.clone(), p));
                    }
                }
            }
        }
        files.sort_by(|a, b| a.1.cmp(&b.1));
        Ok(files)
    }

    /// 按 session_id 找到该会话的 JSONL 文件（id 即文件名去掉 `.jsonl`）。
    fn find_session_file(&self, session_id: &str) -> Result<PathBuf> {
        for (_, path) in self.list_session_files()? {
            if path.file_stem().map(|s| s == session_id).unwrap_or(false) {
                return Ok(path);
            }
        }
        Err(Error::NotFound(format!("session {session_id}")))
    }
}

/// 读一个 JSONL 文件的全部事件（跳过空行与坏 JSON）。
fn read_events(path: &Path) -> Result<Vec<serde_json::Value>> {
    let content = fs::read_to_string(path).map_err(|e| Error::Open {
        path: path.display().to_string(),
        cause: e.to_string(),
    })?;

    let mut events = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str(line) {
            Ok(v) => events.push(v),
            Err(_) => {
                tracing::warn!(file = %path.display(), "skipping malformed JSONL line");
            }
        }
    }
    Ok(events)
}

/// 把 `2026-11-02T02:08:07.053Z` 解析成 Unix 毫秒。
///
/// Claude Code 的时间戳是固定宽度的 RFC3339（毫秒 + `Z`）。手写解析比拖进
/// `chrono` / `time` 划算：本 crate 只需要这一个格式。
fn parse_timestamp_ms(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' {
        return None;
    }
    let num = |from: usize, to: usize| -> Option<i64> { s.get(from..to)?.parse().ok() };

    let year = num(0, 4)?;
    let month = num(5, 7)?;
    let day = num(8, 10)?;
    let hour = num(11, 13)?;
    let minute = num(14, 16)?;
    let second = num(17, 19)?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    let millis: i64 = s
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
/// 从事件流里抽出一条会话的元数据。没有 `user` / `assistant` 事件时返回
/// `None`（空壳会话，与 codebuddy 的口径一致）。
fn summarize(
    events: &[serde_json::Value],
    project_id: &str,
    session_id: &str,
) -> Option<SessionMeta> {
    let mut title = String::new();
    let mut directory = String::new();
    let mut model: Option<String> = None;
    let mut time_created: Option<i64> = None;
    let mut time_updated: i64 = 0;
    let mut tokens_input: i64 = 0;
    let mut tokens_output: i64 = 0;
    let mut tokens_reasoning: i64 = 0;
    let mut cost: Option<f64> = None;
    let mut message_count = 0i64;
    // 已计入 token 的 assistant `message.id`（同一消息多行只算一次）。
    let mut seen_messages: std::collections::HashSet<String> = std::collections::HashSet::new();

    for ev in events {
        let ts = ev
            .get("timestamp")
            .and_then(|t| t.as_str())
            .and_then(parse_timestamp_ms);

        // `cwd` 只在 user / assistant / system 这类带上下文的行上有值。
        if directory.is_empty()
            && let Some(cwd) = ev.get("cwd").and_then(|c| c.as_str())
        {
            directory = cwd.to_string();
        }

        if let Some(ts) = ts {
            time_created = Some(time_created.map_or(ts, |c: i64| c.min(ts)));
            time_updated = time_updated.max(ts);
        }

        match ev.get("type").and_then(|t| t.as_str()) {
            Some("user") => {
                message_count += 1;
                // 只有纯文本用户消息能当标题：tool_result 也是 user 事件，
                // 拿它当标题会得到一段 JSON 残片。
                if title.is_empty()
                    && let Some(text) = ev
                        .get("message")
                        .and_then(|m| m.get("content"))
                        .and_then(|c| c.as_str())
                {
                    let trimmed: String = text.trim().chars().take(80).collect();
                    if !trimmed.is_empty() {
                        title = trimmed.replace('\n', " ");
                    }
                }
            }
            Some("assistant") => {
                message_count += 1;
                let msg = ev.get("message");
                if model.is_none()
                    && let Some(m) = msg.and_then(|m| m.get("model")).and_then(|m| m.as_str())
                    && m != "<synthetic>"
                {
                    model = Some(m.to_string());
                }
                // 同一条消息被拆成 N 行时，这 N 行的 usage **完全相同**（实测
                // `1309/183` 连刷 6 行），所以按 message.id 去重，只算一次。
                // 但不同消息的 usage 是**各自请求的用量**，必须累加。
                let msg_id = msg
                    .and_then(|m| m.get("id"))
                    .and_then(|m| m.as_str())
                    .unwrap_or_default();
                if let Some(usage) = msg
                    .and_then(|m| m.get("usage"))
                    .filter(|_| seen_messages.insert(msg_id.to_string()))
                {
                    let get = |path: &[&str]| -> i64 {
                        let mut cur = usage;
                        for key in path {
                            match cur.get(*key) {
                                Some(v) => cur = v,
                                None => return 0,
                            }
                        }
                        cur.as_i64().unwrap_or(0)
                    };
                    tokens_input += get(&["input_tokens"]);
                    tokens_output += get(&["output_tokens"]);
                    tokens_reasoning += get(&["output_tokens_details", "thinking_tokens"]);
                }
            }
            Some("cost-state") => {
                if let Some(c) = ev.get("totalCostUSD").and_then(|c| c.as_f64()) {
                    cost = Some(cost.map_or(c, |prev: f64| prev.max(c)));
                }
            }
            _ => {}
        }
    }

    if message_count == 0 {
        return None;
    }

    Some(SessionMeta {
        app: APP_NAME.to_string(),
        id: session_id.to_string(),
        project_id: project_id.to_string(),
        project_path: if directory.is_empty() {
            project_id.to_string()
        } else {
            directory.clone()
        },
        parent_id: None,
        title: if title.is_empty() {
            format!("Claude session {session_id}")
        } else {
            title
        },
        directory,
        agent: None,
        model,
        time_created: time_created.unwrap_or(0),
        time_updated,
        tokens_input,
        tokens_output,
        tokens_reasoning,
        cost,
        files_changed: 0,
        archived_at: None,
    })
}
impl SessionSource for ClaudeSource {
    fn app(&self) -> &'static str {
        APP_NAME
    }

    fn projects(&self) -> Result<Vec<ProjectMeta>> {
        // 同一个 cwd 理论上可能对应多个编码目录，按路径聚合。
        let mut by_path: HashMap<String, ProjectMeta> = HashMap::new();

        for (project_id, file) in self.list_session_files()? {
            let Ok(events) = read_events(&file) else {
                continue;
            };
            let stem = file
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            let Some(meta) = summarize(&events, &project_id, &stem) else {
                continue;
            };

            let entry = by_path
                .entry(meta.project_path.clone())
                .or_insert_with(|| ProjectMeta {
                    app: APP_NAME.to_string(),
                    id: meta.project_id.clone(),
                    path: meta.project_path.clone(),
                    vcs: None,
                    session_count: 0,
                    subagent_count: 0,
                    archived_count: 0,
                    time_active: Some(meta.time_updated),
                });
            entry.session_count += 1;
            entry.time_active = Some(
                entry
                    .time_active
                    .map_or(meta.time_updated, |t| t.max(meta.time_updated)),
            );
        }

        let mut out: Vec<ProjectMeta> = by_path.into_values().collect();
        out.sort_by(|a, b| {
            b.time_active
                .unwrap_or(0)
                .cmp(&a.time_active.unwrap_or(0))
                .then(a.path.cmp(&b.path))
        });
        Ok(out)
    }

    fn project_paths_like(&self, pattern: &str) -> Result<Vec<String>> {
        let needle = pattern.to_lowercase();
        let mut out: Vec<String> = self
            .projects()?
            .into_iter()
            .filter(|p| p.path.to_lowercase().contains(&needle))
            .map(|p| p.path)
            .collect();
        out.dedup();
        Ok(out)
    }

    fn directories(&self) -> Result<Vec<DirectoryMeta>> {
        let mut by_dir: HashMap<String, (i64, i64)> = HashMap::new();

        for (project_id, file) in self.list_session_files()? {
            let Ok(events) = read_events(&file) else {
                continue;
            };
            let stem = file
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            let Some(meta) = summarize(&events, &project_id, &stem) else {
                continue;
            };
            if meta.directory.is_empty() {
                continue;
            }
            let entry = by_dir.entry(meta.directory).or_insert((0, 0));
            entry.0 += 1;
            entry.1 = entry.1.max(meta.time_updated);
        }

        let mut out: Vec<DirectoryMeta> = by_dir
            .into_iter()
            .map(|(path, (n, updated))| DirectoryMeta {
                path,
                session_count: n,
                project_count: 1,
                time_updated: updated,
            })
            .collect();
        out.sort_by_key(|d| std::cmp::Reverse(d.time_updated));
        Ok(out)
    }

    fn sessions(&self, filter: &SessionFilter, sort: SortKey) -> Result<Vec<SessionMeta>> {
        let filter_paths: Option<Vec<String>> = match &filter.project {
            ProjectSelector::All => None,
            ProjectSelector::Exact { path } => Some(vec![path.clone()]),
            ProjectSelector::Like { paths, .. } if paths.is_empty() => return Ok(Vec::new()),
            ProjectSelector::Like { paths, .. } => Some(paths.clone()),
        };

        let mut out: Vec<SessionMeta> = Vec::new();

        for (project_id, file) in self.list_session_files()? {
            let Ok(events) = read_events(&file) else {
                continue;
            };
            let stem = file
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            let Some(meta) = summarize(&events, &project_id, &stem) else {
                continue;
            };

            if let Some(ref paths) = filter_paths
                && !paths.iter().any(|p| {
                    meta.project_path == *p || meta.project_path.starts_with(&format!("{p}/"))
                })
            {
                continue;
            }
            if let Some(dir) = &filter.directory
                && !meta.directory.contains(dir)
            {
                continue;
            }
            if let Some(title) = &filter.title_contains
                && !meta.title.to_lowercase().contains(&title.to_lowercase())
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
        let Ok(file) = self.find_session_file(session_id) else {
            return Ok(None);
        };
        let project_id = file
            .parent()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let Ok(events) = read_events(&file) else {
            return Ok(None);
        };
        Ok(summarize(&events, &project_id, session_id))
    }

    /// 导出该会话的**全部原始数据**（无损）。
    ///
    /// 整个 JSONL 文件原样进 `events` 数组，不解析、不挑事件类型：
    /// ```json
    /// { "file": "<uuid>.jsonl", "events": [ ... 全部行 ... ] }
    /// ```
    fn dump(&self, session_id: &str) -> Result<serde_json::Value> {
        let file = self.find_session_file(session_id)?;
        let events = read_events(&file)?;
        let file_name = file
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        Ok(serde_json::json!({
            "file": file_name,
            "events": events,
        }))
    }

    /// 把事件流渲染成 Markdown（见 [`markdown`] 模块的归并规则）。
    fn markdown(&self, session_id: &str) -> Result<String> {
        let dump = self.dump(session_id)?;
        let messages = markdown::from_dump(&dump);
        let meta = self
            .session(session_id)?
            .ok_or_else(|| Error::NotFound(session_id.to_string()))?;
        Ok(aa_session_core::markdown::render(&meta, &messages))
    }

    /// 登录态。
    ///
    /// Claude Code 的凭据不在 `~/.claude` 里，而是环境变量或 OAuth 登录后的
    /// 系统 keychain。本实现**只检查环境变量**，不读 keychain、不返回凭据本体，
    /// 因此结果里只有「有没有配置」和 base URL。
    fn auth(&self, _show_secrets: bool) -> Result<serde_json::Value> {
        let base_url = std::env::var("ANTHROPIC_BASE_URL").ok();
        let has_token = std::env::var_os("ANTHROPIC_AUTH_TOKEN").is_some()
            || std::env::var_os("ANTHROPIC_API_KEY").is_some();
        Ok(serde_json::json!({
            "app": APP_NAME,
            "status": if has_token { "configured" } else { "unknown" },
            "note": "只检查 ANTHROPIC_AUTH_TOKEN / ANTHROPIC_API_KEY；OAuth 凭据在系统 keychain，不读取。",
            "has_token": has_token,
            "base_url": base_url,
            "data_dir": self.root.display().to_string(),
        }))
    }
}

/// 应用排序（与 codebuddy 的实现同口径）。
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
    fn parses_rfc3339_millis() {
        assert_eq!(
            parse_timestamp_ms("2026-11-02T02:08:07.053Z"),
            Some(1_793_585_287_053)
        );
        assert_eq!(parse_timestamp_ms("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(
            parse_timestamp_ms("2024-02-29T12:00:00.500Z"),
            Some(1_709_208_000_500)
        );
        assert_eq!(parse_timestamp_ms("2026-13-02T00:00:00.000Z"), None);
        assert_eq!(parse_timestamp_ms("not a timestamp"), None);
    }

    #[test]
    fn epoch_days_matches_known_dates() {
        assert_eq!(epoch_days(1970, 1, 1), 0);
        assert_eq!(epoch_days(2000, 1, 1), 10_957);
        assert_eq!(epoch_days(2026, 11, 2), 20_759);
    }
}
#[cfg(test)]
mod fixture_tests {
    use super::*;
    use std::io::Write;

    /// `SessionFilter::default()` 的 `limit` 是 0（一条都不要），测试里要显式放开。
    fn no_limit_filter() -> SessionFilter {
        SessionFilter {
            limit: aa_session_core::NO_LIMIT,
            ..Default::default()
        }
    }

    /// 造一个临时数据目录：`projects/<project_id>/<session_id>.jsonl`。
    fn temp_source(files: &[(&str, &str, &str)]) -> (PathBuf, ClaudeSource) {
        let base = std::env::temp_dir().join(format!(
            "aa-session-claude-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&base).unwrap();
        for (project, session, body) in files {
            let dir = base.join(PROJECTS_DIR).join(project);
            fs::create_dir_all(&dir).unwrap();
            let mut f = fs::File::create(dir.join(format!("{session}.jsonl"))).unwrap();
            f.write_all(body.as_bytes()).unwrap();
        }
        let src = ClaudeSource::open_dir(&base).unwrap();
        (base, src)
    }

    const SAMPLE: &str = concat!(
        "{\"type\":\"mode\",\"uuid\":null}",
        "\n",
        r#"{"type":"user","uuid":"u1","cwd":"/w/proj","timestamp":"2026-11-02T02:08:07.053Z","message":{"role":"user","content":"第一条问题"}}"#,
        "\n",
        r#"{"type":"assistant","uuid":"a1","cwd":"/w/proj","timestamp":"2026-11-02T02:08:09.000Z","message":{"id":"m1","model":"<synthetic>","usage":{"input_tokens":0,"output_tokens":0},"content":[{"type":"text","text":"x"}]}}"#,
        "\n",
        r#"{"type":"system","subtype":"turn_duration","uuid":"s1","cwd":"/w/proj","timestamp":"2026-11-02T02:08:09.500Z","messageCount":2}"#,
        "\n",
        r#"{"type":"assistant","uuid":"a2","cwd":"/w/proj","timestamp":"2026-11-02T02:08:10.000Z","message":{"id":"m2","model":"claude-x","usage":{"input_tokens":100,"output_tokens":10},"content":[{"type":"text","text":"y"}]}}"#,
        "\n",
        "   ",
        "\n",
        r#"not json at all"#,
        "\n",
        r#"{"type":"assistant","uuid":"a3","cwd":"/w/proj","timestamp":"2026-11-02T02:08:11.000Z","message":{"id":"m2","model":"claude-x","usage":{"input_tokens":100,"output_tokens":10},"content":[{"type":"tool_use","id":"t1","name":"Read","input":{}}]}}"#,
        "\n",
        r#"{"type":"user","uuid":"u2","cwd":"/w/proj","timestamp":"2026-11-02T02:08:12.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}"#,
        "\n",
        r#"{"type":"mode","uuid":"m"}"#,
        "\n",
        r#"{"type":"cost-state","totalCostUSD":0.5}"#,
        "\n",
        r#"{"type":"cost-state","totalCostUSD":0.25}"#,
        "\n",
    );

    #[test]
    fn lists_project_and_session() {
        let (dir, src) = temp_source(&[("-w-proj", "sess-1", SAMPLE)]);

        let projects = src.projects().unwrap();
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].path, "/w/proj");
        assert_eq!(projects[0].session_count, 1);

        let scenes = src
            .sessions(&no_limit_filter(), SortKey::UpdatedDesc)
            .unwrap();
        assert_eq!(scenes.len(), 1, "scenes={scenes:?}");
        let s = &scenes[0];
        assert_eq!(s.id, "sess-1");
        assert_eq!(s.directory, "/w/proj");
        assert_eq!(s.model.as_deref(), Some("claude-x"));
        // 标题取第一条纯文本 user 消息，不能被 tool_result 抢走。
        assert_eq!(s.title, "第一条问题");
        // 同 message.id 的多行 usage 只算一次；不同消息累加。
        assert_eq!(s.tokens_input, 100);
        assert_eq!(s.tokens_output, 10);
        // 成本取最大（末次累计值）。
        assert_eq!(s.cost, Some(0.5));
        // 坏行与空行被跳过，不影响时间。
        assert_eq!(s.time_created, 1_793_585_287_053);

        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn empty_session_is_hidden() {
        let (dir, src) = temp_source(&[("-w-empty", "sess-empty", r#"{"type":"mode"}"#)]);
        assert!(
            src.sessions(&no_limit_filter(), SortKey::UpdatedDesc)
                .unwrap()
                .is_empty()
        );
        assert!(src.projects().unwrap().is_empty());
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn dump_keeps_every_event_verbatim() {
        let (dir, src) = temp_source(&[("-w-proj", "sess-1", SAMPLE)]);
        let dump = src.dump("sess-1").unwrap();
        assert_eq!(dump["file"], "sess-1.jsonl");
        // 坏行被跳过，其余原样保留（含运行时状态事件）。
        let events = dump["events"].as_array().unwrap();
        // 10 行有效事件（空行与 `not json` 被跳过）。
        assert_eq!(events.len(), 10);
        assert!(events.iter().any(|e| e["type"] == "cost-state"));
        assert_eq!(
            src.dump("nope").unwrap_err().to_string(),
            "not found: session nope"
        );
        fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn project_like_matches_substring() {
        let (dir, src) = temp_source(&[("-w-proj", "sess-1", SAMPLE)]);
        assert_eq!(
            src.project_paths_like("proj").unwrap(),
            vec!["/w/proj".to_string()]
        );
        assert!(src.project_paths_like("nope").unwrap().is_empty());
        fs::remove_dir_all(dir).ok();
    }
}
