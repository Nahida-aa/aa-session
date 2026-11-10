//! opencode 会话数据源（只读，直读 SQLite）。
//!
//! 目标库：`~/.local/share/opencode/opencode.db`（SQLite/WAL，约 6 GB）。
//!
//! 读 `session_v2` / `session_message` / `project`（v2 schema），v1 的
//! `session` / `message` / `part` 作为历史数据补充。
//!
//! # 两套 schema
//!
//! 打开时探测 `session_v2` 是否存在：
//!
//! - **存在**（当前 opencode）：以 v2 为准，v1 里未被迁走的旧会话用
//!   `NOT IN (SELECT id FROM session_v2)` 补进结果；
//! - **不存在**（老版本、以及实测的 Windows 构建）：整库只有 v1，所有查询
//!   切到 `session` / `message` / `part`，**一个 `session_v2` 都不能引用**，
//!   否则直接报 `no such table: session_v2`。
//!
//! # 为什么直读而不是走 HTTP API
//!
//! opencode 提供 `opencode serve`（`GET /session` 等）。不用它的原因：
//!
//! 1. **它要求 opencode 进程在跑**，而我们的核心场景恰恰是关着 opencode
//!    时翻查历史会话；
//! 2. 要取全部消息正文得逐个 session 发 HTTP 请求（当前 891 个会话、
//!    20 万条消息），而本地一次 SQL 查询即可；
//! 3. 写路径本来就不该碰——那正是与运行中进程抢状态。
//!
//! # 只读打开
//!
//! 以 SQLite `mode=ro` URI 打开，**永不写入**。

pub mod backup;
pub mod markdown;
pub mod rename;

use std::path::{Path, PathBuf};

use aa_session_core::{
    DirectoryMeta, Error, ProjectMeta, ProjectSelector, Result, SessionFilter, SessionMeta,
    SessionSource, SortKey,
};
use rusqlite::{Connection, OpenFlags, Row, params};

/// 默认的 opencode 数据目录（相对 `$HOME`）。
pub const DEFAULT_DATA_DIR: &str = ".local/share/opencode";

/// 数据库文件名。
pub const DB_FILE_NAME: &str = "opencode.db";

/// 本数据源的应用名。
pub const APP_NAME: &str = "opencode";

/// 会话列（v2）。与 [`SESSION_FROM_PROJECT`] 的列序保持一致。
const SESSION_COLUMNS: &str = "s.id, s.project_id, COALESCE(p.worktree, ''), s.parent_id, \
                              s.title, s.directory, s.agent, s.model, \
                              s.time_created, s.time_updated, s.tokens_input, s.tokens_output, \
                              s.tokens_reasoning, s.cost, COALESCE(s.summary_files, 0), \
                              s.time_archived";

/// 会话列（v1）。**列序与 [`SESSION_COLUMNS`] 完全一致**，
/// 好让 [`session_from_row`] 只写一遍。v1 没有 `fork_*`/`workspace_id` 列，
/// 也没有 `summary_files`（用 0 占位，保持列数不变）。
const SESSION_COLUMNS_V1: &str = "s.id, s.project_id, COALESCE(p.worktree, ''), s.parent_id, \
                                 s.title, s.directory, s.agent, s.model, \
                                 s.time_created, s.time_updated, s.tokens_input, s.tokens_output, \
                                 s.tokens_reasoning, s.cost, 0, s.time_archived";

/// v2 会话的来源表：从 project 表 join 出 `project_path`。
const SESSION_FROM_PROJECT: &str = "FROM session_v2 s \
                                   LEFT JOIN project p ON p.id = s.project_id";

/// v1 会话的来源表。
///
/// **去重**（排除已在 v2 里出现的会话）不放在这里 —— 这里只能有
/// `FROM ... JOIN`，加 `WHERE` 会和 [`build_where`] 生成的 `WHERE 1 = 1 ...`
/// 撞成双 `WHERE`。去重条件由 [`build_where_with`] 在 v1 分支的 WHERE 子句里
/// 追加 `s.id NOT IN (SELECT id FROM session_v2)`。
const SESSION_FROM_PROJECT_V1: &str = "FROM session s \
                                       LEFT JOIN project p ON p.id = s.project_id";

/// opencode 的只读数据源。
pub struct OpencodeSource {
    conn: Connection,
    /// 打开时解析出的绝对路径，仅用于错误信息。
    path: PathBuf,
    /// 库里是否存在 v2 的 `session_v2` 表。
    ///
    /// 老版本 opencode 只写 v1 的 `session` / `message` / `part`，v2 表根本
    /// 不存在（实测 Windows 上的库就是这样）。所有查询都按这个开关在 v1 / v2
    /// 之间**二选一**，不能假设两套表并存——那会直接报
    /// `no such table: session_v2`。
    has_v2: bool,
}

impl OpencodeSource {
    /// 从数据目录（如 `~/.local/share/opencode`）打开。
    pub fn open_dir(dir: &Path) -> Result<Self> {
        Self::open(&dir.join(DB_FILE_NAME))
    }

    /// 从家目录展开后的默认位置打开。
    pub fn open_default() -> Result<Self> {
        let home = aa_session_core::home_dir().ok_or_else(|| {
            Error::InvalidArgument("无法定位用户家目录（HOME / USERPROFILE 均未设置）".into())
        })?;
        Self::open_dir(&home.join(DEFAULT_DATA_DIR))
    }

    /// 以只读模式打开指定数据库文件。
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_impl(path, false)
    }

    /// 以**读写**模式打开指定数据库文件。
    ///
    /// 只给写路径（改名）用；其余一律走 [`Self::open`]。读写连接与只读连接是
    /// 两个不同的构造入口，这样「不小心写了库」在类型层面就不可能发生。
    pub fn open_writable(path: &Path) -> Result<Self> {
        Self::open_impl(path, true)
    }

    fn open_impl(path: &Path, writable: bool) -> Result<Self> {
        let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if !path.exists() {
            return Err(Error::Open {
                path: path.display().to_string(),
                cause: "file does not exist".into(),
            });
        }

        // `mode=ro` 从 SQLite 层保证只读，比打开后再靠代码自律可靠。
        // 写路径用普通 RW 连接，并交给 SQLite 自己管 WAL。
        // URI 的拼装交给 core：Windows 上 `canonicalize()` 给的 `\\?\C:\...`
        // 不能直接进 `file:` URI。
        let uri = aa_session_core::sqlite_file_uri(&path, !writable);
        let flags = if writable {
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_URI
        } else {
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI
        };
        let conn = Connection::open_with_flags(&uri, flags).map_err(|e| Error::Open {
            path: path.display().to_string(),
            cause: e.to_string(),
        })?;

        // 6 GB 的库，缺索引的查询会全表扫。这里只影响速度，不影响正确性。
        let _ = conn.pragma_update(None, "mmap_size", 1_073_741_824); // 1 GiB
        let _ = conn.pragma_update(None, "cache_size", -65_536); // 64 MiB

        // schema 探测只做一次：v1-only 的库每次查询都会撞 `session_v2` 不存在。
        let has_v2 = table_exists(&conn, "session_v2");

        Ok(Self { conn, path, has_v2 })
    }

    /// 数据库文件路径。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 只读连接。改名预检用（全是 `SELECT`）。
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// 可写连接。**只有改名路径能用**。
    pub fn conn_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    /// 读取 opencode 的登录态（`~/.local/share/opencode/auth.json`）。
    ///
    /// 该文件是「provider → 凭据」的映射，两类形态：
    /// - `{"type":"api","key":"sk-..."}` —— 各 provider 的 API key
    /// - `{"type":"oauth","access":"…","refresh":"…","expires":…}` —— OAuth
    ///
    /// 所有密钥一律脱敏（前缀 + 长度），原文件就在 `auth_file` 里，需要原文
    /// 自己 `cat`。文件不存在时 `status == "not_logged_in"`（正常状态）。
    fn read_auth(&self, show_secrets: bool) -> serde_json::Value {
        let auth_file = self
            .path
            .parent()
            .map(|d| d.join("auth.json"))
            .unwrap_or_else(|| PathBuf::from("auth.json"));

        let Ok(text) = std::fs::read_to_string(&auth_file) else {
            return serde_json::json!({
                "app": APP_NAME,
                "status": "not_logged_in",
                "auth_file": auth_file.display().to_string(),
                "note": format!(
                    "没有找到 {} —— 本机没有为 opencode 配置任何 provider 凭据。",
                    auth_file.display()
                ),
            });
        };

        let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&text) else {
            return serde_json::json!({
                "app": APP_NAME,
                "status": "unreadable",
                "auth_file": auth_file.display().to_string(),
                "error": "auth.json 不是合法 JSON",
            });
        };

        let mut providers = serde_json::Map::new();
        if let Some(map) = parsed.as_object() {
            for (name, cred) in map {
                providers.insert(name.clone(), mask_credential(cred, show_secrets));
            }
        }

        serde_json::json!({
            "app": APP_NAME,
            "status": if providers.is_empty() { "not_logged_in" } else { "logged_in" },
            "auth_file": auth_file.display().to_string(),
            "provider_count": providers.len(),
            "providers": providers,
        })
    }

    /// 执行 `SELECT *` 查询，把每行（列名 + 值）原样构造成 JSON 对象。
    ///
    /// `data` 列等 TEXT 值保留为字符串（含内嵌 JSON 串），不做二次解析，
    /// 保证无损。
    fn dump_rows_by_sql(&self, sql: &str, session_id: &str) -> Result<Vec<serde_json::Value>> {
        use rusqlite::types::ValueRef;

        let mut stmt = self.conn().prepare(sql).map_err(query_err)?;
        let col_names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
        let mut rows = stmt.query(params![session_id]).map_err(query_err)?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().map_err(query_err)? {
            let mut obj = serde_json::Map::new();
            for (i, name) in col_names.iter().enumerate() {
                let vref = row.get_ref(i).map_err(query_err)?;
                let val = match vref {
                    ValueRef::Null => serde_json::Value::Null,
                    ValueRef::Integer(n) => serde_json::Value::Number(n.into()),
                    ValueRef::Real(f) => serde_json::Value::Number(
                        serde_json::Number::from_f64(f).unwrap_or(serde_json::Number::from(0)),
                    ),
                    ValueRef::Text(b) => {
                        serde_json::Value::String(String::from_utf8_lossy(b).to_string())
                    }
                    ValueRef::Blob(b) => {
                        serde_json::Value::String(format!("{{hex: {}}}", hex::encode(b)))
                    }
                };
                obj.insert(name.clone(), val);
            }
            out.push(serde_json::Value::Object(obj));
        }
        Ok(out)
    }

    /// 按 schema 分支构造 WHERE 子句。
    ///
    /// v1 分支额外带上「排除已在 v2 里出现的会话」，保证 UNION ALL 之后
    /// 没有重复。v1-only 的库不存在 `session_v2`，这一步整个跳过。
    fn build_where_with(&self, filter: &SessionFilter, is_v1: bool) -> Result<BuildWhereResult> {
        let mut result = build_where(filter, self.has_v2);
        if is_v1 {
            // v1 分支：空壳判定改走 `message` 表（v2 的 `session_message` 对
            // 早期会话没用，那批会话的消息全在 v1 的 `message` 里）。
            result.sql = result
                .sql
                .replace(filter.empty.sql_filter(), filter.empty.sql_filter_v1());
            if self.has_v2 {
                result
                    .sql
                    .push_str(" AND s.id NOT IN (SELECT id FROM session_v2)");
            }
        }
        Ok(result)
    }

    /// `projects()` 用的计数 SQL。
    ///
    /// v1-only 的库没有 `session_v2`：v2 的三个计数列退回常量 0，v1 的计数
    /// 去掉「排除已在 v2 出现」的条件。**列序保持不变**，行映射不用改。
    fn projects_sql(&self) -> String {
        let (v2_top, v2_sub, v2_arch) = if self.has_v2 {
            (
                "(SELECT count(*) FROM session_v2 s \
                   WHERE s.project_id = p.id AND s.parent_id IS NULL \
                     AND s.time_archived IS NULL \
                     AND EXISTS (SELECT 1 FROM session_message m WHERE m.session_id = s.id))",
                "(SELECT count(*) FROM session_v2 s \
                   WHERE s.project_id = p.id AND s.parent_id IS NOT NULL \
                     AND s.time_archived IS NULL)",
                "(SELECT count(*) FROM session_v2 s \
                   WHERE s.project_id = p.id AND s.time_archived IS NOT NULL)",
            )
        } else {
            ("0", "0", "0")
        };
        let v1_not_in_v2 = if self.has_v2 {
            " AND s.id NOT IN (SELECT id FROM session_v2)"
        } else {
            ""
        };
        // v1 的 `project` 表没有 `time_active` 列（实测本机就是如此），
        // 用 `time_updated` 作为最近活跃时间的等价替代。
        let time_active = if self.has_v2 {
            "p.time_active"
        } else {
            "p.time_updated"
        };
        format!(
            "SELECT p.id, COALESCE(p.worktree, ''), p.vcs, \
             {v2_top}, \
             (SELECT count(*) FROM session s \
                WHERE s.project_id = p.id AND s.parent_id IS NULL \
                  AND s.time_archived IS NULL{v1_not_in_v2} \
                  AND EXISTS (SELECT 1 FROM message m WHERE m.session_id = s.id)), \
             {v2_sub}, {v2_arch}, \
             (SELECT count(*) FROM session s \
                WHERE s.project_id = p.id AND s.time_archived IS NOT NULL{v1_not_in_v2}), \
             {time_active} \
             FROM project p \
             ORDER BY {time_active} DESC, p.id ASC"
        )
    }

    /// `directories()` 用的聚合 SQL。
    ///
    /// v1-only 的库没有 `session_v2`：UNION 的 v2 那一支整段去掉，
    /// v1 那一支去掉「排除已在 v2 出现」的条件。
    fn directories_sql(&self) -> String {
        let v2_branch = if self.has_v2 {
            "SELECT s.directory AS dir, s.project_id, 1 AS n, s.time_updated AS updated \
             FROM session_v2 s \
             WHERE s.parent_id IS NULL \
               AND s.time_archived IS NULL \
               AND EXISTS (SELECT 1 FROM session_message m WHERE m.session_id = s.id) \
             UNION ALL "
        } else {
            ""
        };
        let v1_not_in_v2 = if self.has_v2 {
            " AND s.id NOT IN (SELECT id FROM session_v2)"
        } else {
            ""
        };
        format!(
            "SELECT d.dir AS directory, sum(d.n) AS n, max(d.updated) AS updated, \
                    count(DISTINCT d.project_id) AS projects \
             FROM ( {v2_branch}\
                   SELECT s.directory AS dir, s.project_id, 1 AS n, s.time_updated AS updated \
                   FROM session s \
                   WHERE s.parent_id IS NULL \
                     AND s.time_archived IS NULL{v1_not_in_v2} \
                     AND EXISTS (SELECT 1 FROM message m WHERE m.session_id = s.id) \
             ) d \
             GROUP BY d.dir ORDER BY updated DESC"
        )
    }

    /// 按 `worktree` 全等找出 project id；同路径的多个 id 全部返回。
    ///
    /// 返回空 `Vec` 表示该路径下没有项目。改名预检用它确认「源存在、
    /// 目标未被占用」。
    pub fn project_ids_for_path(&self, path: &str) -> Result<Vec<String>> {
        self.collect_rows(
            "SELECT id FROM project WHERE worktree = ?",
            &[&path],
            |row| row.get(0).map_err(query_err),
        )
    }

    /// 执行查询并把行收集成 `Vec`。
    fn collect_rows<T>(
        &self,
        sql: &str,
        binds: &[&dyn rusqlite::ToSql],
        map_row: impl Fn(&Row<'_>) -> Result<T>,
    ) -> Result<Vec<T>> {
        let mut stmt = self.conn.prepare(sql).map_err(query_err)?;
        let mut rows = stmt
            .query(rusqlite::params_from_iter(binds.iter()))
            .map_err(query_err)?;

        let mut out = Vec::new();
        // `Rows` 只实现了 `next()`，不是 `Iterator` —— 必须手写循环。
        while let Some(row) = rows.next().map_err(query_err)? {
            out.push(map_row(row)?);
        }
        Ok(out)
    }
}

/// 把 `rusqlite` 错误转成领域错误。
fn query_err(e: rusqlite::Error) -> Error {
    Error::Query(e.to_string())
}

/// 库里是否存在该表。
///
/// v1-only 的库连 `session_v2` 这个名字都不存在，所有 SQL 必须先问这里，
/// 不能靠「先跑一查，报错了再兜底」——错误信息会混进正常流程。
pub(crate) fn table_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
        params![name],
        |row| row.get::<_, i64>(0),
    )
    .map(|n| n > 0)
    .unwrap_or(false)
}

/// 把一行会话记录读成 [`SessionMeta`]。
///
/// 列序必须与 [`SESSION_COLUMNS`] 一致。
fn session_from_row(row: &Row<'_>) -> Result<SessionMeta> {
    let directory: String = row.get(5).map_err(query_err)?;
    let project_path: String = row.get(2).map_err(query_err)?;

    Ok(SessionMeta {
        app: APP_NAME.to_string(),
        id: row.get(0).map_err(query_err)?,
        project_id: row.get(1).map_err(query_err)?,
        // join 不到 project 行时（理论上不该发生）回落到会话自身的目录，
        // 总比空串有用。
        project_path: if project_path.is_empty() {
            directory.clone()
        } else {
            project_path
        },
        parent_id: row.get(3).map_err(query_err)?,
        // title 实际允许 NULL（与列声明的 NOT NULL 有出入），兜底成空串。
        title: row
            .get::<_, Option<String>>(4)
            .map_err(query_err)?
            .unwrap_or_default(),
        directory,
        agent: row.get(6).map_err(query_err)?,
        model: row.get(7).map_err(query_err)?,
        time_created: row.get(8).map_err(query_err)?,
        time_updated: row.get(9).map_err(query_err)?,
        tokens_input: row.get(10).map_err(query_err)?,
        tokens_output: row.get(11).map_err(query_err)?,
        tokens_reasoning: row.get(12).map_err(query_err)?,
        // opencode 对绝大多数会话写的是 0.0，真正有意义的只有非零值；
        // 把 0 归一成 None，表格里就不用满屏 `$0.00`。
        cost: match row.get::<_, f64>(13).map_err(query_err)? {
            c if c > 0.0 => Some(c),
            _ => None,
        },
        files_changed: row.get(14).map_err(query_err)?,
        archived_at: row.get(15).map_err(query_err)?,
    })
}

/// 转义 LIKE 的通配符，让用户输入被当作字面量。
fn escape_like(input: &str) -> String {
    input
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// WHERE 子句 + 绑定值。
struct BuildWhereResult {
    sql: String,
    binds: Vec<Box<dyn rusqlite::ToSql>>,
}

/// 按过滤条件拼 `WHERE` 子句与绑定值。
///
/// `has_v2` 决定空壳判定要不要带上 v2 的 `session_message` 那一半：
/// v1-only 的库里 `session_message` 可能整表为空（甚至不存在），照抄 v2 条件
/// 会把所有会话都判成空壳、一条都不剩。
fn build_where(filter: &SessionFilter, has_v2: bool) -> BuildWhereResult {
    let mut sql = String::from(" WHERE 1 = 1");
    let mut binds: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

    fn push_in(
        sql: &mut String,
        binds: &mut Vec<Box<dyn rusqlite::ToSql>>,
        col: &str,
        vals: &[String],
    ) {
        if vals.is_empty() {
            return;
        }
        let holes = vals.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        sql.push_str(&format!(" AND {col} IN ({holes})"));
        for v in vals {
            binds.push(Box::new(v.clone()));
        }
    }

    // 子 agent 会话默认排除 —— 见 `Subagents` 的文档。
    sql.push_str(filter.subagents.sql_filter());
    // 归档会话默认排除 —— 见 `Archived` 的文档。
    sql.push_str(filter.archived.sql_filter());
    // 空壳会话（0 消息）默认排除 —— 见 `EmptySessions` 的文档。
    // v2 用 session_message、v1 用 message，两表各判一次；v1-only 的库只判 v1。
    if has_v2 {
        sql.push_str(filter.empty.sql_filter());
    }
    sql.push_str(filter.empty.sql_filter_v1());

    // 项目过滤走 project 表的 worktree，不能直接拿 s.project_id —— 因为
    // 同路径可能有多个 project id（实测 Nahida 那条路径就有 2 个）。
    match &filter.project {
        ProjectSelector::All => {}
        ProjectSelector::Exact { path } => {
            sql.push_str(" AND s.project_id IN (SELECT id FROM project WHERE worktree = ?)");
            binds.push(Box::new(path.clone()));
        }
        ProjectSelector::Like { paths, .. } => {
            // paths 是 **worktree** 不是 id，必须经 project 表再转一层；
            // 直接拿去 IN project_id 会一条都匹配不到。
            let holes = paths.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            sql.push_str(&format!(
                " AND s.project_id IN (SELECT id FROM project WHERE worktree IN ({holes}))"
            ));
            for p in paths {
                binds.push(Box::new(p.clone()));
            }
        }
    }

    push_in(&mut sql, &mut binds, "s.agent", &filter.agents);

    if let Some(dir) = &filter.directory {
        // LIKE 而非 GLOB：路径里的 `*` / `?` 在 GLOB 中是通配符，
        // 而用户想的是子串匹配。
        sql.push_str(" AND s.directory LIKE ? ESCAPE '\\'");
        binds.push(Box::new(format!("%{}%", escape_like(dir))));
    }

    if let Some(t) = &filter.title_contains {
        sql.push_str(" AND s.title LIKE ? ESCAPE '\\' COLLATE NOCASE");
        binds.push(Box::new(format!("%{}%", escape_like(t))));
    }

    if let Some(after) = filter.updated_after {
        sql.push_str(" AND s.time_updated >= ?");
        binds.push(Box::new(after));
    }

    BuildWhereResult { sql, binds }
}

/// 排序子句。都以 `id` 收尾保证**排序稳定**，否则同一批里顺序随机。
fn order_by(sort: SortKey) -> &'static str {
    match sort {
        SortKey::UpdatedDesc => "s.time_updated DESC, s.id ASC",
        SortKey::CreatedDesc => "s.time_created DESC, s.id ASC",
        SortKey::Title => "s.title COLLATE NOCASE ASC, s.id ASC",
        SortKey::Tokens => "(s.tokens_input + s.tokens_output) DESC, s.id ASC",
    }
}

impl SessionSource for OpencodeSource {
    fn app(&self) -> &'static str {
        APP_NAME
    }

    /// 列出所有项目及其会话数。
    ///
    /// `session_count` 只数**顶层会话**，与 [`SessionSource::sessions`] 的默认
    /// 行为保持一致 —— 否则 `projects` 显示 274 而 `sessions` 只列出 100 多条，
    /// 用户会以为哪里坏了。子会话数另放在 `subagent_count`。
    ///
    /// 用 `LEFT JOIN` 而不是 `JOIN`：没有会话的项目也该出现（否则用户会
    /// 以为项目不存在）。
    fn projects(&self) -> Result<Vec<ProjectMeta>> {
        // 会话数对齐 `sessions` 的默认口径（顶层 + 未归档 + 非空壳），
        // 且 v1/v2 都算——去重用「id 不在 session_v2」把重叠的 v1 行排除。
        //
        // v1-only 的库没有 `session_v2`：三个 v2 子查询全部换成常量 0，
        // 列序不变，下面的行映射不用动。
        let sql = self.projects_sql();

        self.collect_rows(&sql, &[], |row| {
            let get = |i: usize| -> Result<i64> { row.get(i).map_err(query_err) };
            Ok(ProjectMeta {
                app: APP_NAME.to_string(),
                id: row.get(0).map_err(query_err)?,
                path: row.get(1).map_err(query_err)?,
                vcs: row.get(2).map_err(query_err)?,
                session_count: get(3)? + get(4)?,
                subagent_count: get(5)?,
                archived_count: get(6)? + get(7)?,
                time_active: row.get::<_, Option<i64>>(8).map_err(query_err)?,
            })
        })
    }

    /// 列出所有会话运行目录（`session_v2.directory` 去重）。
    ///
    /// `project_count` 用 `COUNT(DISTINCT project_id)` 而不是布尔聚合 ——
    /// 一个目录完全可能同时包含本项目的会话和「跑错位置」的会话。
    /// 统计口径对齐 `sessions` 的默认行为（排除子 agent 与归档），否则
    /// `directories` 和 `sessions` 的数字会对不上。
    fn directories(&self) -> Result<Vec<DirectoryMeta>> {
        // 与 `sessions` 同口径：顶层 + 未归档 + 非空壳，v1/v2 都算。
        // UNION ALL 后按目录聚合，`project_count` 用 DISTINCT 跨两表计数。
        // v1-only 的库由 `directories_sql()` 去掉 v2 那一支。
        let sql = self.directories_sql();

        self.collect_rows(&sql, &[], |row| {
            let path: String = row.get(0).map_err(query_err)?;
            Ok(DirectoryMeta {
                // directory 理论上非空，但历史数据里见过空串，别让它显示成空行。
                path: if path.is_empty() {
                    "(未知目录)".to_string()
                } else {
                    path
                },
                session_count: row.get(1).map_err(query_err)?,
                time_updated: row.get(2).map_err(query_err)?,
                project_count: row.get(3).map_err(query_err)?,
            })
        })
    }

    /// 模糊找出 worktree 含该子串的项目（返回 worktree 列表，已去重）。
    ///
    /// 去重是必要的：同路径的两个 project id 会产生两条相同 worktree，
    /// 而下游 `ProjectSelector::Like` 拿的是 worktree 而非 id。
    fn project_paths_like(&self, pattern: &str) -> Result<Vec<String>> {
        let sql = "SELECT DISTINCT worktree FROM project \
                   WHERE worktree LIKE ?1 ESCAPE '\\' ORDER BY worktree";
        let needle = format!("%{}%", escape_like(pattern));
        self.collect_rows(sql, &[&needle], |row| row.get(0).map_err(query_err))
    }

    fn sessions(&self, filter: &SessionFilter, sort: SortKey) -> Result<Vec<SessionMeta>> {
        // 同 id 在 v1 和 v2 都有时以 v2 为准（v1 分支用 NOT IN 排除），
        // 避免同一条会话在结果里出现两次。
        //
        // v1-only 的库没有 `session_v2`：直接只查 v1，不拼 v2 那一支，
        // 否则 SQL 一执行就报 `no such table: session_v2`。
        let (sql, mut binds) = if self.has_v2 {
            let v2_where = self.build_where_with(filter, false)?;
            let v1_where = self.build_where_with(filter, true)?;

            let v2_sql = format!(
                "SELECT {SESSION_COLUMNS} {SESSION_FROM_PROJECT}{}",
                v2_where.sql
            );
            // 同一套列序 + WHERE 片段（`s.` 前缀在两张表都成立），
            // v1 分支只需换来源表。
            let v1_sql = format!(
                "SELECT {SESSION_COLUMNS_V1} {SESSION_FROM_PROJECT_V1}{}",
                v1_where.sql
            );

            let sql = format!(
                "{v2_sql} \
                 UNION ALL \
                 {v1_sql} \
                 ORDER BY {} LIMIT ?",
                order_by(sort)
            );
            let mut all: Vec<Box<dyn rusqlite::ToSql>> = v2_where.binds;
            all.extend(v1_where.binds);
            (sql, all)
        } else {
            let v1_where = self.build_where_with(filter, true)?;
            let sql = format!(
                "SELECT {SESSION_COLUMNS_V1} {SESSION_FROM_PROJECT_V1}{} \
                 ORDER BY {} LIMIT ?",
                v1_where.sql,
                order_by(sort)
            );
            (sql, v1_where.binds)
        };

        binds.push(Box::new(filter.limit as i64));
        let refs: Vec<&dyn rusqlite::ToSql> = binds.iter().map(|b| b.as_ref()).collect();

        self.collect_rows(&sql, &refs, session_from_row)
    }

    fn session(&self, session_id: &str) -> Result<Option<SessionMeta>> {
        // 先查 v2，查不到再查 v1 —— 同 id 的两份记录以 v2 为准。
        if self.has_v2 {
            let v2 = format!("SELECT {SESSION_COLUMNS} {SESSION_FROM_PROJECT} WHERE s.id = ?1");
            if let Some(meta) = self
                .collect_rows(&v2, &[&session_id], session_from_row)?
                .into_iter()
                .next()
            {
                return Ok(Some(meta));
            }
        }

        let v1 = format!("SELECT {SESSION_COLUMNS_V1} {SESSION_FROM_PROJECT_V1} WHERE s.id = ?1");
        Ok(self
            .collect_rows(&v1, &[&session_id], session_from_row)?
            .into_iter()
            .next())
    }

    /// 导出该会话的**全部原始数据**（无损）。
    ///
    /// 查 `session_v2`（v1 兜底 `session`）+ `session_message` + `message`
    /// + `part`，每张表的该会话所有行原样返回，`data` 列不解析（保留 JSON 串）。
    ///
    /// 返回的 JSON 对象含 `session` / `session_message` / `message` / `part` 四个字段。
    fn dump(&self, session_id: &str) -> Result<serde_json::Value> {
        // 会话行：v2 优先，查不到回落到 v1 `session` 表。
        // v1-only 的库没有 `session_v2`，直接读 v1。
        let session = if self.has_v2 {
            self.dump_rows_by_sql("SELECT * FROM session_v2 WHERE id = ?1", session_id)?
                .into_iter()
                .next()
                .or_else(|| {
                    self.dump_rows_by_sql("SELECT * FROM session WHERE id = ?1", session_id)
                        .ok()
                        .and_then(|rows| rows.into_iter().next())
                })
        } else {
            self.dump_rows_by_sql("SELECT * FROM session WHERE id = ?1", session_id)?
                .into_iter()
                .next()
        }
        .ok_or_else(|| Error::NotFound(format!("session {session_id}")))?;

        // v1-only 的库没有 v2 的 `session_message`，这一路留空数组；
        // `markdown::from_dump` 见到空就自动改用 v1 的 `message`。
        let session_messages = if self.has_v2 {
            self.dump_rows_by_sql(
                "SELECT * FROM session_message WHERE session_id = ?1 ORDER BY seq ASC",
                session_id,
            )?
        } else {
            Vec::new()
        };
        let v1_messages = self.dump_rows_by_sql(
            "SELECT * FROM message WHERE session_id = ?1 \
             ORDER BY time_created ASC, id ASC",
            session_id,
        )?;
        let parts = self.dump_rows_by_sql(
            "SELECT * FROM part WHERE session_id = ?1 \
             ORDER BY time_created ASC, id ASC",
            session_id,
        )?;

        Ok(serde_json::json!({
            "session": session,
            "session_message": session_messages,
            "message": v1_messages,
            "part": parts,
        }))
    }

    /// 把会话渲染成 Markdown。
    ///
    /// 先 `dump()` 拿到原始记录，再交给 [`markdown::from_dump`] 解析成统一
    /// 对话模型，最后用 `core::markdown::render` 排版。
    fn markdown(&self, session_id: &str) -> Result<String> {
        let dump = self.dump(session_id)?;
        let messages = markdown::from_dump(&dump);
        let meta = self
            .session(session_id)?
            .ok_or_else(|| Error::NotFound(format!("session {session_id}")))?;
        Ok(aa_session_core::markdown::render(&meta, &messages))
    }

    fn auth(&self, show_secrets: bool) -> Result<serde_json::Value> {
        Ok(self.read_auth(show_secrets))
    }
}

///
/// `api` 类型只留 key 前缀 + 长度；`oauth` 类型对 access / refresh 同样处理，
/// 并把 `expires` 转成时效状态。未知形态原样带过（不含密钥的元信息字段）。
fn mask_credential(cred: &serde_json::Value, show_secrets: bool) -> serde_json::Value {
    let Some(map) = cred.as_object() else {
        return cred.clone();
    };

    let mut out = serde_json::Map::new();
    for (k, v) in map {
        match k.as_str() {
            // 密钥字段：脱敏
            "key" | "access" | "refresh" | "secret" | "token" => {
                if let Some(s) = v.as_str() {
                    // show_secrets 时保留原字段名与原文，脱敏时加 Masked 后缀。
                    let (target, shown) = if show_secrets {
                        (k.clone(), s.to_string())
                    } else {
                        (format!("{k}Masked"), mask_secret(s))
                    };
                    out.insert(target, serde_json::json!(shown));
                    continue;
                }
                out.insert(k.clone(), v.clone());
            }
            // 时效字段：换算成天数
            "expires" => {
                if let Some(ms) = v.as_i64() {
                    out.insert("expiresStatus".into(), serde_json::json!(expiry_status(ms)));
                }
                out.insert(k.clone(), v.clone());
            }
            _ => {
                out.insert(k.clone(), v.clone());
            }
        }
    }
    serde_json::Value::Object(out)
}

/// 密钥脱敏：前缀 + 长度。
fn mask_secret(secret: &str) -> String {
    let chars: Vec<char> = secret.chars().collect();
    if chars.is_empty() {
        return "(空)".to_string();
    }
    let head: String = chars.iter().take(8).collect();
    format!("{head}…（{} 字符）", chars.len())
}

/// 由 Unix 毫秒到期时间算状态。
fn expiry_status(expires_ms: i64) -> serde_json::Value {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let left_ms = expires_ms - now_ms;
    let days_left = left_ms / 86_400_000;
    let state = if left_ms <= 0 {
        "expired"
    } else if days_left <= 7 {
        "expiring_soon"
    } else {
        "valid"
    };
    serde_json::json!({
        "state": state,
        "expires_at": expires_ms,
        "days_left": days_left,
    })
}

#[cfg(test)]
mod schema_tests {
    use super::*;

    fn source(has_v2: bool) -> OpencodeSource {
        OpencodeSource {
            conn: Connection::open_in_memory().expect("内存库"),
            path: PathBuf::new(),
            has_v2,
        }
    }

    #[test]
    fn v1_only_sql_never_mentions_session_v2() {
        // v1-only 的库里 `session_v2` 不存在，任何一句提到它的 SQL 都会
        // 直接把 `projects` / `directories` 打挂。
        let v1 = source(false);
        assert!(!v1.projects_sql().contains("session_v2"));
        assert!(!v1.directories_sql().contains("session_v2"));
    }

    #[test]
    fn v2_sql_keeps_dedup_against_v1() {
        let v2 = source(true);
        assert!(v2.projects_sql().contains("session_v2"));
        assert!(v2.directories_sql().contains("session_v2"));
    }

    #[test]
    fn table_exists_detects_only_real_tables() {
        let src = source(false);
        src.conn
            .execute_batch("CREATE TABLE session (id TEXT)")
            .expect("建表");
        assert!(table_exists(&src.conn, "session"));
        assert!(!table_exists(&src.conn, "session_v2"));
    }
}

#[cfg(test)]
mod auth_tests {
    use super::*;

    #[test]
    fn api_key_is_masked_not_passed_through() {
        let cred = serde_json::json!({"type": "api", "key": "sk-1234567890abcdef"});
        let masked = mask_credential(&cred, false);
        let s = masked.to_string();
        assert!(!s.contains("1234567890abcdef"), "原始 key 不得出现在输出里");
        assert!(s.contains("sk-1234"));
        assert!(s.contains("字符"));
    }

    #[test]
    fn oauth_both_tokens_masked_with_expiry() {
        let future = 1_800_000_000_000i64; // 远处
        let cred = serde_json::json!({
            "type": "oauth",
            "refresh": "ref-abcdefghijklmnop",
            "access": "acc-ABCDEFGHIJKLMNOP",
            "expires": future,
        });
        let masked = mask_credential(&cred, false);
        assert!(!masked.to_string().contains("abcdefghijklmnop"));
        assert_eq!(
            masked["accessMasked"].as_str().unwrap().chars().count(),
            8 + "…（20 字符）".chars().count()
        );
        assert!(!masked["refreshMasked"].as_str().unwrap().is_empty());
        assert_eq!(masked["expiresStatus"]["state"], "valid");
    }

    #[test]
    fn expired_token_is_flagged() {
        let past = 1_000i64;
        let cred = serde_json::json!({"type": "oauth", "access": "x", "expires": past});
        let masked = mask_credential(&cred, false);
        assert_eq!(masked["expiresStatus"]["state"], "expired");
    }

    #[test]
    fn unknown_credential_shape_passes_through() {
        let cred = serde_json::json!({"type": "weird", "note": "no secret here"});
        assert_eq!(mask_credential(&cred, false), cred);
        assert_eq!(mask_credential(&cred, true), cred);
    }

    #[test]
    fn show_secrets_keeps_original_key_and_field_name() {
        let cred = serde_json::json!({"type": "api", "key": "sk-1234567890abcdef"});
        let raw = mask_credential(&cred, true);
        // 原文 + 原字段名，不加 Masked 后缀
        assert_eq!(raw["key"], "sk-1234567890abcdef");
        assert!(raw.get("keyMasked").is_none());
    }

    #[test]
    fn empty_secret_shows_placeholder() {
        let cred = serde_json::json!({"type": "api", "key": ""});
        let masked = mask_credential(&cred, false);
        assert_eq!(masked["keyMasked"], "(空)");
    }
}
