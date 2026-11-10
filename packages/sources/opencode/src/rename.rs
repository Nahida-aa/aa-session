//! 项目改名：把 `project.worktree` 从旧路径改到新路径。
//!
//! # 这是本工具唯一会写 opencode 库的路径
//!
//! 其余所有操作都只读。这里必须格外保守，规则见 [`plan`] 与 [`apply`]。
//!
//! # 改什么、不改什么
//!
//! 路径字符串散布在两类地方，性质完全不同：
//!
//! | 表.列 | 性质 | 改不改 |
//! |---|---|---|
//! | `project.worktree` | 项目记录指针 | ✅ 改 |
//! | `session_v2.directory` / `session.directory` | 会话工作目录指针 | ✅ 改（跟随） |
//! | `message.data` / `part.data` / `session_message.data` / `event.data` | **历史内容** | ❌ 不改 |
//!
//! 历史 blob 里存的是当时真实发生的事，比如 `{"name":"bash","input":{"command":
//! "nohup du ... /old/path ..."}}`。改它等于伪造记录。
//!
//! # 前缀替换的边界
//!
//! 替换只在**目录边界**上生效：`/a/b` → `/a/b-old` 时，
//! `/a/b/c` 变 `/a/b-old/c`，但 `/a/bb` 和 `/a/bx` 必须**原样不动**。
//!
//! ⚠️ 光靠 `LIKE '/a/b%'` **做不到** —— 它会把 `/a/bb` 也匹配上，
//! 于是 `/a/bb` 被改成 `/a/b-oldb`（本模块的单元测试专门盯这个）。
//! 所以匹配条件是「全等 **或** 以 `old + '/'` 开头」两个条件的或，
//! 见 [`under_prefix`]。

use aa_session_core::Result;
use rusqlite::{Connection, ToSql};
use tracing::info;

/// 改名会影响的行数统计。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RenamePlan {
    /// 将被更新的 `project.worktree` 行数。
    pub project_rows: usize,
    /// 将被更新的 `session_v2.directory` 行数。
    pub session_v2_rows: usize,
    /// 将被更新的 `session.directory`（v1 遗留表）行数。
    pub session_rows: usize,
    /// 因新旧路径相同而无须改动的会话行数（已经是新路径）。
    pub already_current: usize,
}

impl RenamePlan {
    /// 本次改名总共会改动多少行。
    pub fn total(&self) -> usize {
        self.project_rows + self.session_v2_rows + self.session_rows
    }

    /// 是否什么都不用改。
    pub fn is_noop(&self) -> bool {
        self.total() == 0
    }
}

/// 预检改名的影响面，不做任何写入。
///
/// `conn` 可以是只读连接 —— 这里全是 `SELECT`。
pub fn plan(conn: &Connection, old_path: &str, new_path: &str) -> Result<RenamePlan> {
    let count = |sql: &str, binds: &[&dyn ToSql]| -> Result<usize> {
        let n: i64 = conn
            .query_row(sql, binds, |r| r.get(0))
            .map_err(crate::query_err)?;
        Ok(n as usize)
    };

    // 前缀用 `LIKE ... ESCAPE` 匹配，转义掉 `%` / `_` 这类通配符。
    // 「目录边界」不靠 SQL 保证，而靠 apply 里的 `substr(col, old.len()+1)`：
    // 旧路径整好等于该值时尾巴为空串，`/a/b/c` 命中时尾巴是 `/c`。
    let needle = format!("{}%", escape_like(old_path));
    let needle_refs = refs(std::slice::from_ref(&needle));

    let project_rows = count(
        "SELECT count(*) FROM project WHERE worktree LIKE ? ESCAPE '\\'",
        &needle_refs,
    )?;
    // v1-only 的库没有 `session_v2`：相关计数按 0 处理，不执行查询。
    let has_v2 = crate::table_exists(conn, "session_v2");
    let session_v2_rows = if has_v2 {
        count(
            "SELECT count(*) FROM session_v2 WHERE directory LIKE ? ESCAPE '\\'",
            &needle_refs,
        )?
    } else {
        0
    };
    let session_rows = count(
        "SELECT count(*) FROM session WHERE directory LIKE ? ESCAPE '\\'",
        &needle_refs,
    )?;
    let already_current = if has_v2 {
        count(
            "SELECT count(*) FROM session_v2 WHERE directory = ?",
            &refs(&[new_path.to_string()]),
        )?
    } else {
        0
    };

    Ok(RenamePlan {
        project_rows,
        session_v2_rows,
        session_rows,
        already_current,
    })
}

/// 执行改名。
///
/// 全程在一个事务里：要么全改，要么什么都不改。中途失败不会留下「项目改了、
/// 会话没改」的半更新状态。
pub fn apply(conn: &mut Connection, old_path: &str, new_path: &str) -> Result<RenamePlan> {
    let tx = conn.transaction().map_err(crate::query_err)?;

    // 再算一次，且**在事务内**算，保证 preflight 和 apply 之间没有别人插队。
    let before = plan(&tx, old_path, new_path)?;

    // 逐表改写「以旧路径为根」的记录。
    // 结果 = 新路径 + `substr(col, old.len()+1)`，后者是旧前缀之后的尾巴
    // （`/a/b` 本身命中时尾巴为空串；`/a/b/c` 命中时尾巴是 `/c`）。
    //
    // WHERE 用 `under_prefix`：全等 **或** 以 `old/` 开头。少了全等那一支就会
    // 把 `/a/bb` 也改掉，产出 `/a/b-oldb`。
    let (pw, pb) = under_prefix("worktree", old_path);
    let tail = format!("substr(worktree, {})", old_path.len() + 1);
    tx.execute(
        &format!("UPDATE project SET worktree = ?1 || {tail} WHERE {pw}"),
        rusqlite::params_from_iter(
            std::iter::once(rusqlite::types::Value::Text(new_path.to_string()))
                .chain(pb.into_iter().map(rusqlite::types::Value::Text)),
        ),
    )
    .map_err(crate::query_err)?;

    let (sd, sdb) = under_prefix("directory", old_path);
    let tail = format!("substr(directory, {})", old_path.len() + 1);
    for table in ["session_v2", "session"] {
        // v1-only 的库里 `session_v2` 不存在，跳过即可，别让整条改名失败。
        if !crate::table_exists(&tx, table) {
            continue;
        }
        tx.execute(
            &format!("UPDATE {table} SET directory = ?1 || {tail} WHERE {sd}"),
            rusqlite::params_from_iter(
                std::iter::once(rusqlite::types::Value::Text(new_path.to_string()))
                    .chain(sdb.iter().cloned().map(rusqlite::types::Value::Text)),
            ),
        )
        .map_err(crate::query_err)?;
    }

    tx.commit().map_err(crate::query_err)?;

    info!(
        old_path,
        new_path,
        projects = before.project_rows,
        sessions = before.session_v2_rows + before.session_rows,
        "rename applied"
    );

    Ok(before)
}

/// 构造「以 `prefix` 为根」的匹配条件（SQL 文本片段 + 绑定值）。
///
/// 返回的片段形如 `(worktree = ?1 OR worktree LIKE ?2 ESCAPE '\\')`。
///
/// **必须有全等那一支**：只靠 `LIKE 'prefix%'` 会把兄弟目录
/// （`/a/bb` 对 `/a/b`）一起匹配上，改名时就会产出 `/a/b-oldb` 这种垃圾值。
fn under_prefix(column: &str, prefix: &str) -> (String, Vec<String>) {
    (
        format!("({column} = ? OR {column} LIKE ? ESCAPE '\\')",),
        vec![prefix.to_string(), format!("{}/%", escape_like(prefix))],
    )
}

/// 转义 LIKE 的通配符。
fn escape_like(input: &str) -> String {
    input
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// 把 `Vec<String>` 借成 `&[&dyn ToSql]`。
fn refs(values: &[String]) -> Vec<&dyn ToSql> {
    values.iter().map(|v| v as &dyn ToSql).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    /// 建一个只有 project / session_v2 / session 三张表的内存库。
    fn fixture() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE project (id TEXT PRIMARY KEY, worktree TEXT NOT NULL);
             CREATE TABLE session_v2 (id TEXT PRIMARY KEY, directory TEXT NOT NULL);
             CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT NOT NULL);",
        )
        .unwrap();
        conn
    }

    fn add(conn: &Connection, table: &str, id: &str, dir: &str) {
        let col = if table == "project" {
            "worktree"
        } else {
            "directory"
        };
        conn.execute(
            &format!("INSERT INTO {table} (id, {col}) VALUES (?1, ?2)"),
            params![id, dir],
        )
        .unwrap();
    }

    /// 改名后某个目录应变成什么。
    fn renamed(conn: &Connection, table: &str, id: &str) -> String {
        let col = if table == "project" {
            "worktree"
        } else {
            "directory"
        };
        conn.query_row(
            &format!("SELECT {col} FROM {table} WHERE id = ?1"),
            params![id],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn rewrites_exact_path_and_subdirectories() {
        let mut conn = fixture();
        add(&conn, "project", "p1", "/a/b");
        add(&conn, "session_v2", "m1", "/a/b");
        add(&conn, "session_v2", "m2", "/a/b/sub/dir");
        add(&conn, "session", "o1", "/a/b/c");

        apply(&mut conn, "/a/b", "/a/b-old").unwrap();

        assert_eq!(renamed(&conn, "project", "p1"), "/a/b-old");
        // 整好命中时尾巴为空串
        assert_eq!(renamed(&conn, "session_v2", "m1"), "/a/b-old");
        // 子目录时尾巴被保留
        assert_eq!(renamed(&conn, "session_v2", "m2"), "/a/b-old/sub/dir");
        assert_eq!(renamed(&conn, "session", "o1"), "/a/b-old/c");
    }

    #[test]
    fn does_not_touch_sibling_paths_sharing_the_prefix() {
        // 这是最容易出错的地方：`/a/bb` 和 `/a/bx` 都以 `/a/b` 开头，
        // 但它们不是 `/a/b` 的子目录，绝不能被改。
        let mut conn = fixture();
        add(&conn, "project", "p1", "/a/b");
        add(&conn, "project", "p2", "/a/bb");
        add(&conn, "project", "p3", "/a/bx");
        add(&conn, "session_v2", "m1", "/a/bb/file");
        add(&conn, "session_v2", "m2", "/a/bx");

        apply(&mut conn, "/a/b", "/a/b-old").unwrap();

        assert_eq!(renamed(&conn, "project", "p1"), "/a/b-old");
        assert_eq!(renamed(&conn, "project", "p2"), "/a/bb", "兄弟目录被误改");
        assert_eq!(renamed(&conn, "project", "p3"), "/a/bx", "兄弟目录被误改");
        assert_eq!(renamed(&conn, "session_v2", "m1"), "/a/bb/file");
        assert_eq!(renamed(&conn, "session_v2", "m2"), "/a/bx");
    }

    #[test]
    fn like_wildcards_in_path_are_treated_literally() {
        // 路径里可能含 `_`（它本身是 LIKE 单字符通配符）。若不转义，
        // `/a/b_c` 会误匹配 `/a/bXc`。
        let mut conn = fixture();
        add(&conn, "project", "p1", "/a/b_c");
        add(&conn, "project", "p2", "/a/bXc");

        apply(&mut conn, "/a/b_c", "/a/d").unwrap();

        assert_eq!(renamed(&conn, "project", "p1"), "/a/d");
        assert_eq!(
            renamed(&conn, "project", "p2"),
            "/a/bXc",
            "`_` 被当成通配符了"
        );
    }

    #[test]
    fn percent_in_path_is_literal() {
        let mut conn = fixture();
        add(&conn, "project", "p1", "/a/100%x");
        add(&conn, "project", "p2", "/a/100zzx");

        apply(&mut conn, "/a/100%x", "/a/d").unwrap();

        assert_eq!(renamed(&conn, "project", "p1"), "/a/d");
        assert_eq!(renamed(&conn, "project", "p2"), "/a/100zzx");
    }

    #[test]
    fn plan_reports_row_counts_without_writing() {
        let conn = fixture();
        add(&conn, "project", "p1", "/a/b");
        add(&conn, "session_v2", "m1", "/a/b/x");
        add(&conn, "session", "o1", "/a/b/y");

        let p = plan(&conn, "/a/b", "/a/b-old").unwrap();
        assert_eq!(p.project_rows, 1);
        assert_eq!(p.session_v2_rows, 1);
        assert_eq!(p.session_rows, 1);
        assert_eq!(p.total(), 3);
        assert!(!p.is_noop());

        // plan 必须是纯读
        assert_eq!(renamed(&conn, "project", "p1"), "/a/b");
    }

    #[test]
    fn plan_is_noop_when_nothing_matches() {
        let conn = fixture();
        add(&conn, "project", "p1", "/other");
        let p = plan(&conn, "/a/b", "/a/b-old").unwrap();
        assert!(p.is_noop());
        assert_eq!(p.total(), 0);
    }
}
