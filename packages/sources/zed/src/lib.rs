//! Zed 会话数据源（只读，直读 `~/.local/share/zed/threads/threads.db`）。
//!
//! zed 把线程（会话）存在**一个** SQLite 文件里：
//! `threads` 表，`data` 列是 zstd 压缩的 DbThread JSON，`data_type = "zstd"`。
//!
//! # 与 opencode 的差异
//!
//! - 没有 `project` 表，项目归属靠 `folder_paths` 列（**换行分隔**的多路径，
//!   一个线程可能跨多个项目目录）
//! - `parent_id` 全库实测为 NULL（zed 的子会话上下文在解压后的
//!   `subagent_context` 字段里，本 v1 不解析它，只透传父 id 列）
//! - 时间戳是 ISO 8601 文本（`created_at` / `updated_at`），不是 Unix 毫秒
//! - 消息**不拆表**，整包在压缩 JSON 里（`messages` 数组，元素是
//!   `{"User":…}` / `{"Agent":…}` / `{"Compaction":…}` / 裸串 `"Resume"`）
//! - 无归档、无空壳概念（空壳过滤退化为「0 条消息」判定）
//!
//! # zstd 解压注意
//!
//! zed 用 `zstd::encode_all`（level 3）压缩，帧头**没带 content size**，
//! 所以 `ZstdDecompressor::decode`（需要 FCS）解不开，必须用**流式**
//! `stream_reader().read()`。Rust 里用 `zstd::stream::read::Decoder`。
//!
//! # 只读打开
//!
//! `mode=ro` 打开（threads.db 可能正被运行中的 zed 持有 WAL 连接）。

use std::path::{Path, PathBuf};

use aa_session_core::{
    DirectoryMeta, Error, ImportReport, ProjectMeta, ProjectSelector, Result, SessionFilter,
    SessionImporter, SessionMeta, SessionSource, SortKey,
};
use rusqlite::{Connection, OpenFlags, Row};

pub mod import;
pub mod markdown;

/// zed 的 threads.db 相对 `$HOME` 的路径。
pub const DEFAULT_DATA_DIR: &str = ".local/share/zed";
pub const DB_FILE_NAME: &str = "threads/threads.db";
pub const APP_NAME: &str = "zed";

/// Zed 的只读数据源。
pub struct ZedSource {
    conn: Connection,
    path: PathBuf,
}

impl ZedSource {
    /// 从家目录展开后的默认位置打开。
    pub fn open_default() -> Result<Self> {
        let home = aa_session_core::home_dir().ok_or_else(|| {
            Error::InvalidArgument("无法定位用户家目录（HOME / USERPROFILE 均未设置）".into())
        })?;
        Self::open(&home.join(DEFAULT_DATA_DIR).join(DB_FILE_NAME))
    }

    /// 从数据目录（`~/.local/share/zed`）打开。
    pub fn open_dir(dir: &Path) -> Result<Self> {
        Self::open(&dir.join(DB_FILE_NAME))
    }

    /// 以只读模式打开指定数据库文件。
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_impl(path, false)
    }

    /// 以**读写**模式打开指定数据库文件。
    ///
    /// 只给导入（写库）路径用；其余一律走 [`Self::open`]。
    /// 两个构造入口分开，「不小心写了库」在类型层面就不可能。
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

        let _ = conn.pragma_update(None, "mmap_size", 256 * 1024 * 1024);
        let _ = conn.pragma_update(None, "cache_size", -16_384); // 16 MiB

        Ok(Self { conn, path })
    }

    /// 可写连接。**只有导入路径能用**（与 opencode 的 `conn_mut` 同一契约）。
    pub fn conn_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    /// 覆盖导入一个 export 文件（同 `id` 整行替换）。
    ///
    /// 写库前的安全契约由调用方（CLI `import` 子命令）负责：
    /// 预检 + `--apply` + 写前备份。这里只做「把 `raw_data` 写回 `threads` 表」。
    ///
    /// `columns` 缺列（老导出文件没有 `folder_paths_order`）时该列写 NULL，
    /// zed 反序列化时 `order: Vec::new()` 等价于不排序，行为正确。
    pub(crate) fn import_overwrite(
        &mut self,
        thread_id: &str,
        columns: &serde_json::Value,
        thread_json: &serde_json::Value,
    ) -> Result<usize> {
        let tj = thread_json;
        let cols = columns;
        let get = |k: &str| cols.get(k).and_then(|v| v.as_str()).map(str::to_string);
        let id: String = cols
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or(thread_id)
            .to_string();
        let summary = get("summary").unwrap_or_else(|| {
            tj.get("title")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string()
        });
        let updated_at = get("updated_at").unwrap_or_else(|| {
            tj.get("updated_at")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string()
        });
        let created_at = get("created_at").unwrap_or_else(|| updated_at.clone());
        let parent_id = cols
            .get("parent_id")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let folder_paths = cols
            .get("folder_paths")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let folder_paths_order = cols
            .get("folder_paths_order")
            .and_then(|v| v.as_str())
            .map(str::to_string);

        let data = compress_thread_json(tj)?;
        let conn = self.conn_mut();
        let mut stmt = conn
            .prepare(
                "INSERT INTO threads (id, summary, updated_at, data_type, data, parent_id, folder_paths, folder_paths_order, created_at)
                 VALUES (?1, ?2, ?3, 'zstd', ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(id) DO UPDATE SET
                   summary = excluded.summary,
                   updated_at = excluded.updated_at,
                   data_type = excluded.data_type,
                   data = excluded.data,
                   parent_id = excluded.parent_id,
                   folder_paths = excluded.folder_paths,
                   folder_paths_order = excluded.folder_paths_order,
                   created_at = excluded.created_at",
            )
            .map_err(query_err)?;
        let n = stmt
            .execute(rusqlite::params![
                id,
                summary,
                updated_at,
                data,
                parent_id,
                folder_paths,
                folder_paths_order,
                created_at,
            ])
            .map_err(query_err)?;
        Ok(n)
    }

    /// 数据库文件路径。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 执行查询并把行收集成 `Vec`。`binds` 是 `?` 占位符的值（可为空）。
    pub(crate) fn collect_rows<T>(
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
        while let Some(row) = rows.next().map_err(query_err)? {
            out.push(map_row(row)?);
        }
        Ok(out)
    }

    /// 某线程 id 是否已存在于库中（导入前判「覆盖」还是「新增」）。
    pub(crate) fn thread_exists(&self, thread_id: &str) -> Result<bool> {
        let n: i64 = self
            .collect_rows(
                "SELECT count(*) FROM threads WHERE id = ?1",
                &[&thread_id],
                |row| row.get::<_, i64>(0).map_err(query_err),
            )?
            .pop()
            .unwrap_or(0);
        Ok(n > 0)
    }
}

/// 解压 zed 的 zstd blob。
///
/// zed 帧头不带 content size（`zstd::encode_all` 默认），所以必须走
/// `Decoder::new(&blob)` + `read_to_end` 的流式路径。
fn decompress_zstd(blob: &[u8]) -> Result<Vec<u8>> {
    use std::io::Read;

    let mut out = Vec::new();
    {
        let mut frame = zstd::stream::read::Decoder::new(blob)
            .map_err(|cause| Error::Query(cause.to_string()))?;
        frame
            .read_to_end(&mut out)
            .map_err(|cause| Error::Query(format!("decompress: {cause}")))?;
    }
    Ok(out)
}

/// 压缩 DbThread JSON（与 zed 的 `zstd::encode_all(level 3)` 一致）。
fn compress_thread_json(json: &serde_json::Value) -> Result<Vec<u8>> {
    let json_bytes = serde_json::to_vec(json).map_err(|cause| Error::Query(cause.to_string()))?;
    let mut out = Vec::new();
    {
        let mut encoder = zstd::stream::write::Encoder::new(&mut out, 3)
            .map_err(|cause| Error::Query(format!("zstd compress: {cause}")))?;
        use std::io::Write;
        encoder
            .write_all(&json_bytes)
            .map_err(|cause| Error::Query(cause.to_string()))?;
        encoder
            .finish()
            .map_err(|cause| Error::Query(format!("zstd finish: {cause}")))?;
    }
    Ok(out)
}

/// 把 `rusqlite` 错误转成领域错误。
pub(crate) fn query_err(e: rusqlite::Error) -> Error {
    Error::Query(e.to_string())
}

/// 解析 zed 的 `folder_paths` 列（**换行分隔**的多路径）。
///
/// zed 源码里是 `PathList::serialize()` = `paths.join("\n")`。
/// 空串 / NULL 返回空 `Vec`。
fn split_folder_paths(raw: Option<&str>) -> Vec<String> {
    let Some(s) = raw else {
        return Vec::new();
    };
    s.split('\n')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .collect()
}

/// ISO 8601 → Unix 毫秒（UTC 墙钟，忽略时区偏移取本地时间戳数值）。
///
/// zed 的 `created_at` / `updated_at` 形如
/// `2026-10-14T17:22:51.762010507+00:00`。手工解析（不引 chrono）。
///
/// 解析失败（字段缺失 / 不是数字）一律返回 0，让排序 / 过滤退化成
/// 「未知时间」，而不是整条会话作废。
fn parse_zed_timestamp(s: &str) -> i64 {
    // 先剥掉时区后缀（`+00:00` / `-05:00` / `Z`）。
    let dt = s
        .split_once('+')
        .or_else(|| s.split_once('Z'))
        .map(|(a, _)| a)
        .unwrap_or(s);

    // 按 `2026-10-14T17:22:51.762010507` 拆。
    // 段序：年 月 日 时 分 秒[.小数]。
    let parts: Vec<&str> = dt.split(['-', 'T', ':']).collect();
    if parts.len() < 6 {
        return 0;
    }

    let parse = |p: &str| p.parse::<i64>().unwrap_or(0);
    let year = parse(parts[0]);
    let month = parse(parts[1]);
    let day = parse(parts[2]);
    let hour = parse(parts[3]);
    let minute = parse(parts[4]);
    // 最后一段可能是 `51` 或 `51.762010507`。
    let (sec, ms) = match parts[5].split_once('.') {
        Some((s, frac)) => {
            let sec = s.parse::<i64>().unwrap_or(0);
            // 小数部分可能是 3~9 位，取前 3 位当毫秒，不足补 0。
            let padded = format!("{frac:3}");
            let ms: i64 = padded
                .chars()
                .take(3)
                .collect::<String>()
                .parse()
                .unwrap_or(0);
            (sec, ms)
        }
        None => (parts[5].parse().unwrap_or(0), 0),
    };

    let days = days_from_civil(year, month, day);
    days.saturating_mul(86_400)
        .saturating_add(hour.saturating_mul(3600))
        .saturating_add(minute.saturating_mul(60))
        .saturating_add(sec)
        .saturating_mul(1000)
        .saturating_add(ms)
}

/// 民历（proleptic Gregorian）day number → 自 1970-01-01 起的天数。
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Unix 毫秒 → zed 风格的 ISO 8601 UTC 文本（`2026-01-01T00:00:00+00:00`）。
fn iso8601_from_ms(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);

    let civil = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}+00:00",
        civil.0,
        civil.1,
        civil.2,
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

/// 自 1970-01-01 起的天数 → 民历 (year, month, day)。
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 36_524) / 365;
    let y = era * 400 + yoe;
    let doy = doe - (yoe * 365 + yoe / 4 - yoe / 100 + yoe / 36_524);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 转义 LIKE 的通配符，让用户输入被当作字面量。
fn escape_like(input: &str) -> String {
    input
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}
///
/// 精确语义 = 该路径是会话涉及的目录之一（任意一段等于它）。
/// 在 Rust 里做，避免 SQLite 单引号字面量里 `\n` 的坑。
fn matches_project_exact(folder_paths_raw: &Option<String>, path: &str) -> bool {
    split_folder_paths(folder_paths_raw.as_deref())
        .iter()
        .any(|seg| seg == path)
}

impl SessionSource for ZedSource {
    fn app(&self) -> &'static str {
        APP_NAME
    }

    /// 列出所有项目及其会话数。
    ///
    /// zed 没有 project 表，项目 = `folder_paths` 第一段的去重结果。
    fn projects(&self) -> Result<Vec<ProjectMeta>> {
        let rows: Vec<(Option<String>, String)> =
            self.collect_rows("SELECT folder_paths, updated_at FROM threads", &[], |row| {
                Ok((
                    row.get::<_, Option<String>>(0).map_err(query_err)?,
                    row.get::<_, String>(1).map_err(query_err)?,
                ))
            })?;

        let mut by_dir: std::collections::BTreeMap<String, (i64, Option<i64>)> =
            std::collections::BTreeMap::new();
        for (folder_paths, updated_at) in &rows {
            let dir = split_folder_paths(folder_paths.as_deref())
                .first()
                .cloned()
                .unwrap_or_else(|| "(未知目录)".to_string());
            let updated_ms = parse_zed_timestamp(updated_at);
            let entry = by_dir.entry(dir).or_insert((0, None));
            entry.0 += 1;
            entry.1 = Some(entry.1.map_or(updated_ms, |v| v.max(updated_ms)));
        }

        let mut out: Vec<ProjectMeta> = by_dir
            .into_iter()
            .map(|(path, (n, time_active))| ProjectMeta {
                app: APP_NAME.to_string(),
                id: path.clone(),
                path,
                vcs: None,
                session_count: n,
                subagent_count: 0,
                archived_count: 0,
                time_active,
            })
            .collect();
        out.sort_by(|a, b| {
            b.time_active
                .unwrap_or(0)
                .partial_cmp(&a.time_active.unwrap_or(0))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        Ok(out)
    }

    /// 模糊找出 worktree 含该子串的项目路径（已去重）。
    fn project_paths_like(&self, pattern: &str) -> Result<Vec<String>> {
        let projects = self.projects()?;
        let needle = pattern.to_lowercase();
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for p in &projects {
            if p.path.to_lowercase().contains(&needle) && seen.insert(p.path.clone()) {
                out.push(p.path.clone());
            }
        }
        Ok(out)
    }

    /// 列出所有**会话运行目录**。
    ///
    /// zed 里一个线程可跨多个目录（`folder_paths` 是列表），所以
    /// `project_count` 表示「涉及该目录的不同线程数」。
    fn directories(&self) -> Result<Vec<DirectoryMeta>> {
        let rows: Vec<(String, Option<String>, String)> = self.collect_rows(
            "SELECT id, folder_paths, updated_at FROM threads",
            &[],
            |row| {
                Ok((
                    row.get::<_, String>(0).map_err(query_err)?,
                    row.get::<_, Option<String>>(1).map_err(query_err)?,
                    row.get::<_, String>(2).map_err(query_err)?,
                ))
            },
        )?;

        let mut by_dir: std::collections::BTreeMap<
            String,
            (i64, i64, std::collections::HashSet<String>),
        > = std::collections::BTreeMap::new();
        for (id, folder_paths, updated_at) in rows {
            let updated_ms = parse_zed_timestamp(&updated_at);
            for dir in split_folder_paths(folder_paths.as_deref()) {
                let entry = by_dir
                    .entry(dir)
                    .or_insert((0, 0, std::collections::HashSet::new()));
                entry.0 += 1;
                entry.1 = updated_ms.max(entry.1);
                entry.2.insert(id.clone());
            }
        }

        let mut out: Vec<DirectoryMeta> = by_dir
            .into_iter()
            .map(|(path, (n, updated, threads))| DirectoryMeta {
                path: if path.is_empty() {
                    "(未知目录)".to_string()
                } else {
                    path
                },
                session_count: n,
                time_updated: updated,
                project_count: threads.len() as i64,
            })
            .collect();
        out.sort_by_key(|d| std::cmp::Reverse(d.time_updated));
        Ok(out)
    }

    fn sessions(&self, filter: &SessionFilter, sort: SortKey) -> Result<Vec<SessionMeta>> {
        let (where_clause, binds) = build_where(filter);
        let sql = format!(
            "SELECT id, summary, updated_at, data, parent_id, folder_paths, created_at \
             FROM threads{where_clause} \
             ORDER BY {} LIMIT ?",
            order_by(sort)
        );

        let mut all: Vec<Box<dyn rusqlite::ToSql>> = binds;
        all.push(Box::new(filter.limit as i64));
        let refs: Vec<&dyn rusqlite::ToSql> = all.iter().map(|b| b.as_ref()).collect();

        let rows = self.collect_rows(&sql, &refs, |row| {
            Ok((
                row.get::<_, String>(0).map_err(query_err)?,
                row.get::<_, String>(1).map_err(query_err)?,
                row.get::<_, String>(2).map_err(query_err)?,
                row.get::<_, Vec<u8>>(3).map_err(query_err)?,
                row.get::<_, Option<String>>(4).map_err(query_err)?,
                row.get::<_, Option<String>>(5).map_err(query_err)?,
                row.get::<_, String>(6).map_err(query_err)?,
            ))
        })?;

        let mut out: Vec<SessionMeta> = Vec::new();
        for (id, summary, updated_at, data, parent_id, folder_paths, created_at) in rows {
            let data = decompress_zstd(&data)
                .ok()
                .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
                .unwrap_or(serde_json::Value::Null);

            // 空壳过滤（0 条消息）。
            let has_messages = data
                .get("messages")
                .and_then(|m| m.as_array())
                .is_some_and(|m| !m.is_empty());
            match filter.empty {
                aa_session_core::EmptySessions::Exclude if !has_messages => continue,
                aa_session_core::EmptySessions::Only if has_messages => continue,
                _ => {}
            }

            // 精确项目过滤（在 Rust 里做，避开 SQLite 换行字面量坑）。
            if let ProjectSelector::Exact { path } = &filter.project
                && !matches_project_exact(&folder_paths, path)
            {
                continue;
            }

            out.push(thread_from_row(
                id,
                summary,
                updated_at,
                data,
                parent_id,
                folder_paths,
                created_at,
            ));
        }

        apply_sort(&mut out, sort);
        Ok(out)
    }

    /// 读取单个线程（解压成 [`SessionMeta`]）。
    fn session(&self, session_id: &str) -> Result<Option<SessionMeta>> {
        let rows = self.collect_rows(
            "SELECT id, summary, updated_at, data, parent_id, folder_paths, created_at \
              FROM threads WHERE id = ?1",
            &[&session_id],
            |row| {
                Ok((
                    row.get::<_, String>(0).map_err(query_err)?,
                    row.get::<_, String>(1).map_err(query_err)?,
                    row.get::<_, String>(2).map_err(query_err)?,
                    row.get::<_, Vec<u8>>(3).map_err(query_err)?,
                    row.get::<_, Option<String>>(4).map_err(query_err)?,
                    row.get::<_, Option<String>>(5).map_err(query_err)?,
                    row.get::<_, String>(6).map_err(query_err)?,
                ))
            },
        )?;

        let Some((id, summary, updated_at, data, parent_id, folder_paths, created_at)) =
            rows.into_iter().next()
        else {
            return Ok(None);
        };

        let data = decompress_zstd(&data)
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .unwrap_or(serde_json::Value::Null);

        Ok(Some(thread_from_row(
            id,
            summary,
            updated_at,
            data,
            parent_id,
            folder_paths,
            created_at,
        )))
    }

    /// 导出该线程的**全部原始数据**（无损）。
    ///
    /// `columns` 是 `threads` 表该行的全部列（`data` 列排除，因为它就是
    /// `thread_json` 的压缩形式），`thread_json` 是解压后的完整 DbThread。
    fn dump(&self, session_id: &str) -> Result<serde_json::Value> {
        let row = self.collect_rows(
            "SELECT id, summary, updated_at, data, parent_id, folder_paths, folder_paths_order, created_at, data_type \
              FROM threads WHERE id = ?1",
            &[&session_id],
            |r| {
                Ok((
                    r.get::<_, String>(0).map_err(query_err)?,
                    r.get::<_, String>(1).map_err(query_err)?,
                    r.get::<_, String>(2).map_err(query_err)?,
                    r.get::<_, Vec<u8>>(3).map_err(query_err)?,
                    r.get::<_, Option<String>>(4).map_err(query_err)?,
                    r.get::<_, Option<String>>(5).map_err(query_err)?,
                    r.get::<_, Option<String>>(6).map_err(query_err)?,
                    r.get::<_, String>(7).map_err(query_err)?,
                    r.get::<_, String>(8).map_err(query_err)?,
                ))
            },
        )?;

        let Some((
            id,
            summary,
            updated_at,
            data,
            parent_id,
            folder_paths,
            folder_paths_order,
            created_at,
            data_type,
        )) = row.into_iter().next()
        else {
            return Err(Error::NotFound(format!(
                "thread {session_id} not found in threads.db"
            )));
        };

        let thread_json_bytes =
            decompress_zstd(&data).map_err(|cause| Error::Query(format!("zstd: {cause}")))?;
        let thread_json: serde_json::Value =
            serde_json::from_slice(&thread_json_bytes).map_err(|cause| Error::MalformedJson {
                location: format!("thread:{id}"),
                cause,
            })?;

        Ok(serde_json::json!({
            "columns": {
                "id": id,
                "summary": summary,
                "updated_at": updated_at,
                "created_at": created_at,
                "parent_id": parent_id,
                "folder_paths": folder_paths,
                "folder_paths_order": folder_paths_order,
                "data_type": data_type,
            },
            "thread_json": thread_json,
        }))
    }

    /// 把线程渲染成 Markdown（见 [`markdown`] 模块的映射规则）。
    fn markdown(&self, session_id: &str) -> Result<String> {
        let dump = self.dump(session_id)?;
        let messages = markdown::from_dump(&dump);
        let meta = self.session(session_id)?.ok_or_else(|| {
            Error::NotFound(format!("thread {session_id} not found in threads.db"))
        })?;
        Ok(aa_session_core::markdown::render(&meta, &messages))
    }

    fn auth(&self, _show_secrets: bool) -> Result<serde_json::Value> {
        // zed 的 provider 凭据（api key / OAuth）不在 threads.db 里，
        // 也不在 settings.json 里（那里只有 model 选择）。实测
        // `~/.local/share/zed/db/*/db.sqlite` 没有 auth 表，copilot 目录
        // 无凭据。先报 not_logged_in + 说明，深挖另开任务。
        Ok(serde_json::json!({
            "app": APP_NAME,
            "status": "not_logged_in",
            "note": "zed 的 LLM provider 凭据不在已知位置（threads.db / settings.json / db.sqlite 均无 auth 表，copilot 目录无凭据）。深挖 zed 的 auth 存储尚未定位，见 ROADMAP。",
        }))
    }
}

impl SessionImporter for ZedSource {
    fn importer_name(&self) -> &'static str {
        "zed (threads.db)"
    }

    fn import(&mut self, export_path: &std::path::Path, apply: bool) -> Result<ImportReport> {
        crate::import::run(self, export_path, apply)
    }
}

/// 把 `threads` 表的一行（+ 解压后的 JSON）读成 [`SessionMeta`]。
fn thread_from_row(
    id: String,
    summary: String,
    updated_at: String,
    data: serde_json::Value,
    parent_id: Option<String>,
    folder_paths_raw: Option<String>,
    created_at: String,
) -> SessionMeta {
    let folders = split_folder_paths(folder_paths_raw.as_deref());
    let primary = folders.first().cloned().unwrap_or_default();

    // 项目 id：zed 没有 project 表，用主目录绝对路径当 id。
    let project_id = primary.clone();

    // 解压后的 JSON 里能拿到真实 token 用量（`cumulative_token_usage`）。
    let (tokens_input, tokens_output, tokens_reasoning) = data
        .get("cumulative_token_usage")
        .map(|u| {
            (
                u.get("input_tokens").and_then(|v| v.as_i64()).unwrap_or(0),
                u.get("output_tokens").and_then(|v| v.as_i64()).unwrap_or(0),
                u.get("reasoning_tokens")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0),
            )
        })
        .unwrap_or((0, 0, 0));

    let model = data
        .get("model")
        .and_then(|m| m.get("id"))
        .and_then(|i| i.as_str())
        .map(str::to_string);

    SessionMeta {
        app: APP_NAME.to_string(),
        id,
        project_id,
        project_path: primary.clone(),
        parent_id,
        title: summary,
        directory: primary,
        agent: None,
        model,
        time_created: parse_zed_timestamp(&created_at),
        time_updated: parse_zed_timestamp(&updated_at),
        tokens_input,
        tokens_output,
        tokens_reasoning,
        cost: None,
        files_changed: 0,
        archived_at: None,
    }
}

/// 排序子句（与 opencode 同风格，`id` 收尾保证稳定）。
fn order_by(sort: SortKey) -> &'static str {
    match sort {
        SortKey::UpdatedDesc => "updated_at DESC, id ASC",
        SortKey::CreatedDesc => "created_at DESC, id ASC",
        SortKey::Title => "summary COLLATE NOCASE ASC, id ASC",
        // 没有 tokens 列（在压缩 blob 里），退化为数据量大的在前。
        SortKey::Tokens => "length(data) DESC, id ASC",
    }
}

/// 排序后在内存里做最终排序（token 排序需要解压数据，无法在 SQL 里做）。
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

/// 按 `SessionFilter` 拼 `WHERE` 子句与绑定值。
///
/// 全部在 SQL 层做（不解压 blob），所以 `folder_paths` / `summary` /
/// `updated_at` 等列都能用。
fn build_where(filter: &SessionFilter) -> (String, Vec<Box<dyn rusqlite::ToSql>>) {
    let mut sql = String::from(" WHERE 1 = 1");
    let mut binds: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

    // 子 agent：zed 实测全库 parent_id 均 NULL，但保留语义。
    // （核心里的 `sql_filter()` 带 opencode 的 `s.` 表别名，这里手写。）
    match filter.subagents {
        aa_session_core::Subagents::Exclude => sql.push_str(" AND parent_id IS NULL"),
        aa_session_core::Subagents::Include => {}
        aa_session_core::Subagents::Only => sql.push_str(" AND parent_id IS NOT NULL"),
    }
    // 归档：zed 没有归档概念，`time_archived` 列不存在，忽略。

    match &filter.project {
        ProjectSelector::All => {}
        ProjectSelector::Exact { path } => {
            // 精确匹配（「该路径是会话涉及的目录之一」）在 Rust 行循环里做，
            // 不进 SQL：`folder_paths` 换行分隔 + SQLite 单引号字面量里的
            // `\n` 是反斜杠 + n（不是换行），拼 SQL 极易写错（实测踩过）。
            // 见 `sessions()` 里的 `matches_project_exact` 检查。
            let _ = path;
        }
        ProjectSelector::Like { paths, .. } => {
            // 模糊匹配走 SQL 子串 LIKE（不含换行，没有字面量坑）。
            if paths.is_empty() {
                sql.push_str(" AND 1 = 0");
            } else if paths.len() == 1 {
                sql.push_str(" AND folder_paths LIKE ? ESCAPE '\\'");
                binds.push(Box::new(format!("%{}%", escape_like(&paths[0]))));
            } else {
                sql.push_str(" AND (");
                for p in paths {
                    sql.push_str("folder_paths LIKE ? ESCAPE '\\'");
                    binds.push(Box::new(format!("%{}%", escape_like(p))));
                }
                sql.push(')');
            }
        }
    }

    if let Some(dir) = &filter.directory {
        sql.push_str(" AND COALESCE(folder_paths, '') LIKE ? ESCAPE '\\'");
        binds.push(Box::new(format!("%{dir}%")));
    }

    if let Some(t) = &filter.title_contains {
        sql.push_str(" AND summary LIKE ? ESCAPE '\\' COLLATE NOCASE");
        binds.push(Box::new(format!("%{t}%")));
    }

    if let Some(after) = filter.updated_after {
        sql.push_str(" AND updated_at >= ?");
        // zed 存的是 ISO 8601 文本，`>=` 比较在零填充的 UTC 时间戳下成立。
        binds.push(Box::new(iso8601_from_ms(after)));
    }

    (sql, binds)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_folder_paths_splits_on_newline() {
        let s = "a\nb\n\nc";
        assert_eq!(split_folder_paths(Some(s)), vec!["a", "b", "c"]);
        assert_eq!(split_folder_paths(None), Vec::<String>::new());
        assert_eq!(split_folder_paths(Some("")), Vec::<String>::new());
    }

    #[test]
    fn parse_zed_timestamp_handles_fractional_seconds() {
        let ms = parse_zed_timestamp("2026-10-14T17:22:51.762010507+00:00");
        // 2026-10-14 的 unix 秒应该在 1.8e9 ~ 2.0e9 范围内（对应 2027 年前后）
        assert!(
            (1_700_000_000_000..2_000_000_000_000).contains(&ms),
            "应在 2026~2027 年内，实际 {ms}"
        );
        // 毫秒部分应该是 762（`.762`）
        assert_eq!(ms % 1000, 762, "毫秒部分应为 762，实际 {}", ms % 1000);
    }

    #[test]
    fn parse_zed_timestamp_returns_zero_on_garbage() {
        assert_eq!(parse_zed_timestamp("garbage"), 0);
        assert_eq!(parse_zed_timestamp(""), 0);
    }

    #[test]
    fn iso8601_from_ms_matches_zed_format() {
        assert_eq!(iso8601_from_ms(0), "1970-01-01T00:00:00+00:00");
        assert_eq!(iso8601_from_ms(86_400_000), "1970-01-02T00:00:00+00:00");
    }

    #[test]
    fn parse_zed_round_trips_with_iso8601() {
        let ts = "2026-10-14T17:22:51.762+00:00";
        let parsed = parse_zed_timestamp(ts);
        // 重新格式化后应能再次解析出相同毫秒（截到秒 + 毫秒）
        let reformatted = iso8601_from_ms(parsed / 1000 * 1000);
        assert_eq!(parse_zed_timestamp(&reformatted), parsed / 1000 * 1000);
    }
}
