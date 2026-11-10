//! 本地 agent 会话的领域模型与数据源抽象。
//!
//! 这一层不依赖任何具体 agent 的存储格式，也不依赖任何 UI 框架。
//! 上层（CLI、将来的 GUI）只面向这里的 [`SessionMeta`] / [`ProjectMeta`] 和
//! [`SessionSource`] trait 编程。原始会话数据的导出由各 source crate 的
//! `dump()` 方法直接返回 `serde_json::Value`（原样无损），不走统一抽象。
//!
//! # 关于 `app`
//!
//! [`SessionSource::app`] 标识这条数据**由哪个 agent 应用产生**（opencode /
//! claude-code / codex ...）。它同时也是「来源」和「目标」——将来做 fork 时，
//! 「从 app A 的会话 fork 出 app A 的新会话」用的是同一个字段。

pub mod error;
pub mod home;
pub mod markdown;
pub mod model;
pub mod source;

pub use error::{Error, Result};
pub use home::{home_dir, long_path, sqlite_file_uri};
pub use model::{DirectoryMeta, ProjectMeta, SessionMeta, UNKNOWN_PROJECT_PATH};
pub use source::{
    Archived, EmptySessions, ImportReport, NO_LIMIT, ProjectSelector, SessionFilter,
    SessionImporter, SessionSource, SortKey, Subagents,
};
