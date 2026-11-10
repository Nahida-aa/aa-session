//! `aa-session` —— 列出并导出本地 agent 会话。
//!
//! 设计前提：**不做全文搜索**。891 个会话的列举是一次 SQL 查询的事，而全文
//! 检索交给用户在自己喜欢的编辑器里做——先把会话导出成可读 JSON，再用 Zed /
//! IDE 的搜索。这样我们不必维护索引，也不必猜用户想要什么样的检索。

mod export;
mod format;
mod output;
mod table;

use export::{ExportFormat, assemble};
use format::Format;
use output::{human_bytes, out};

use std::path::{Path, PathBuf};

use aa_session_core::{
    Archived, EmptySessions, NO_LIMIT, ProjectSelector, SessionFilter, SessionImporter,
    SessionMeta, SessionSource, SortKey, Subagents,
};
use aa_session_source_claude::ClaudeSource;
use aa_session_source_codebuddy::CodebuddySource;
use aa_session_source_codebuddy_ide::CodebuddyIdeSource;
use aa_session_source_opencode::OpencodeSource;
use aa_session_source_opencode::{backup, rename};
use aa_session_source_zed::ZedSource;
use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use tracing_subscriber::EnvFilter;

/// 顶层参数。
#[derive(Debug, Parser)]
#[command(
    name = "aa-session",
    version,
    about = "列出并导出本地 agent 会话",
    before_help = BEFORE_HELP
)]
struct Args {
    /// 操作哪个 agent 应用的数据（必填，不猜）。
    ///
    /// 这里不能用 clap 的 `required`：全局必填参数在 clap 里两种位置都解析不到，
    /// 只能写成可选 + 下面手动校验，好处是能给出可操作的错误提示。
    // 只给长选项：`-a` 已分配给 `sessions --agent`，clap 的 debug_assert
    // 会在两个短选项冲突时直接 panic（只有同时用到两个参数才触发，
    // 很容易漏测）。
    #[arg(long, global = true, value_enum)]
    app: Option<AppArg>,

    /// 覆盖该 app 的数据库路径（默认由 app 自身决定）。
    #[arg(long, global = true, value_name = "PATH")]
    db: Option<PathBuf>,

    /// 日志级别（`RUST_LOG` 同格式，默认 `warn`）。
    #[arg(long, global = true, value_name = "FILTER")]
    log: Option<String>,

    #[command(subcommand)]
    command: Command,
}

const BEFORE_HELP: &str = "\
`--app` 是必填的 —— 本工具只读某个 agent 应用自己的存储，不猜目标。

典型流程
  1. 看看有哪些项目          aa-session --app opencode projects
  2. 挑一个项目看会话        aa-session --app opencode sessions --project-like LocalDub
  3. 导出单个会话            aa-session --app opencode export <SESSION_ID> -o chat.json
  4. 导出某个目录的会话      aa-session --app opencode export --directory LocalDub -o exports/LocalDub/opencode
  5. 在编辑器里全文搜索      zed exports/LocalDub/opencode   （目录可直接打开并跨文件搜索）

`export` 只有一条命令：给 SESSION_ID 就导一个文件，不给就按筛选条件批量导到
文件夹（一会话一文件 + `_index.json`）。

`--format json`（默认）是原始记录的完整镜像（含工具调用的参数与结果），保留原始
字符串，可直接被编辑器的搜索命中；`--format markdown` 是版式化的对话
（`## User` / `## Assistant` + 工具调用与结果），给人读。两者单会话都可能达数十
MB，批量导出前先 `--dry-run` 看清单。
批量导出默认跳过已存在的同名文件，可以反复重跑补增量（`--force` 覆盖）。

默认只列出**顶层**且**未归档**的会话。opencode 会把派出的探查任务记成子会话
（本库 891 个会话里 652 个是子会话），它们默认隐藏，需要时用
--subagents include|only。同理归档会话默认隐藏，用 --archived include|only。

本工具默认只读；唯一会写库的子命令是 rename-project（默认预检 + 写前备份）。
";

#[derive(Debug, Subcommand)]
enum Command {
    /// 列出所有项目及其会话数。
    Projects {
        /// 只列会话数 >= 该值的项目。
        #[arg(short = 'n', long, value_name = "N")]
        min_sessions: Option<i64>,

        /// 输出格式。
        #[arg(long, short = 'f', value_enum, default_value_t = Format::Table)]
        format: Format,

        /// 等价于 `--format json`（保留旧用法）。
        #[arg(long)]
        json: bool,
    },

    /// 列出会话。
    Sessions {
        /// 最多列出多少条。
        #[arg(short, long, default_value_t = 30)]
        limit: usize,

        /// 只列该项目的会话。**只接受绝对路径，全等匹配** `project.worktree`。
        ///
        /// 模糊匹配（目录名、部分路径）请用 `--project-like`。
        #[arg(long = "project", value_name = "ABS_PATH")]
        project: Option<String>,

        /// 按项目路径**子串**模糊过滤（多命中是预期行为）。
        ///
        /// 与 `--project` 互斥。
        #[arg(long = "project-like", value_name = "SUBSTR")]
        project_like: Option<String>,

        /// 只列工作目录包含该子串的会话。
        #[arg(long, value_name = "SUBSTR")]
        directory: Option<String>,

        /// 只列这些 agent，可重复（如 build / plan）。
        #[arg(short, long = "agent", value_name = "NAME")]
        agents: Vec<String>,

        /// 只列标题包含该子串的会话（大小写不敏感）。
        #[arg(short = 't', long = "title", value_name = "SUBSTR")]
        title: Option<String>,

        /// 只列最近 N 分钟内更新过的会话。
        #[arg(long, value_name = "MINUTES")]
        since: Option<i64>,

        /// 子 agent 会话（opencode 派出的探查任务）的纳入方式。
        #[arg(long, value_enum, default_value_t = SubArg::Exclude, value_name = "MODE")]
        subagents: SubArg,

        /// 归档会话的纳入方式。归档是用户主动收起的会话，默认不显示。
        #[arg(long, value_enum, default_value_t = ArchArg::Exclude, value_name = "MODE")]
        archived: ArchArg,

        /// 空壳会话（一条消息都没有的占位会话）的纳入方式。
        ///
        /// opencode 会为「打开过但从未使用」的会话建行：0 消息、0 token，
        /// 标题是 `New session - <ISO 时间戳>`。opencode 的 UI 不显示它们，
        /// 我们默认也不显示。全库顶层未归档会话里 **21%** 是这种空壳。
        #[arg(long, value_enum, default_value_t = EmptyArg::Exclude, value_name = "MODE")]
        empty: EmptyArg,

        /// 排序键。
        #[arg(long, value_enum, default_value_t = SortArg::Updated)]
        sort: SortArg,

        /// 输出格式：`table`（对齐表格，给人扫）/ `json` / `toon`（省 token）。
        #[arg(long, short = 'f', value_enum, default_value_t = Format::Table)]
        format: Format,

        /// 等价于 `--format json`（保留旧用法）。
        #[arg(long)]
        json: bool,
    },

    /// 把项目的路径改成新的（**唯一会写 opencode 库的子命令**）。
    ///
    /// 默认只做预检并打印将要发生的改动，加 `--apply` 才真正写入。
    /// 仅改「指针」字段（`project.worktree`、`session.directory`），
    /// **不改**消息历史里的路径（那是当时实际发生的事）。
    RenameProject {
        /// 旧路径（绝对路径，全等匹配）。
        old_path: String,

        /// 新路径（绝对路径）。
        new_path: String,

        /// 真正写入。不加这个只做预检。
        #[arg(long)]
        apply: bool,

        /// 允许新路径在磁盘上不存在（默认要求存在，避免打错字）。
        #[arg(long)]
        allow_missing: bool,

        /// 备份文件路径（默认与数据库同目录、文件名带时间戳，每次都是新文件）。
        #[arg(long, value_name = "PATH")]
        backup_to: Option<PathBuf>,
    },

    /// 列出所有**会话运行目录**及其会话数。
    ///
    /// 会话的运行目录和它归属的项目**不是一回事**（见下方说明），所以这是
    /// 一个独立视图，而不是 `projects` 的重复。
    Directories {
        /// 只列包含该子串的目录。
        #[arg(long, value_name = "SUBSTR")]
        directory: Option<String>,

        /// 只列会话数 >= 该值的目录。
        #[arg(short = 'n', long, value_name = "N")]
        min_sessions: Option<i64>,

        /// 输出格式。
        #[arg(long, short = 'f', value_enum, default_value_t = Format::Table)]
        format: Format,

        /// 等价于 `--format json`（保留旧用法）。
        #[arg(long)]
        json: bool,
    },

    /// 把一个会话的全部原始数据导出到标准输出。
    ///
    /// 默认输出 JSON（可直接管道给 `jq`）；`--format markdown` 输出
    /// 版式化的对话（`## User` / `## Assistant`），适合直接看或贴进 issue。
    Show {
        /// 会话 id。
        session_id: String,

        /// 输出格式：`json`（原始记录）/ `markdown`（版式化对话）。
        #[arg(short = 'f', long, value_enum, default_value_t = ExportFormat::Json)]
        format: ExportFormat,
    },

    /// 导出会话：给 SESSION_ID 导一个文件；不给则按筛选条件批量导出到文件夹。
    ///
    /// 两种用法是同一条命令，**区别只在于给不给 SESSION_ID**：
    ///
    /// `aa-session --app opencode export <SESSION_ID> -o chat.json`
    ///
    /// `aa-session --app opencode export --directory LocalDub -o exports/LocalDub/opencode`
    ///
    /// 批量模式的产物是给人读的文件树：一会话一文件，文件名形如
    /// `2026-10-24_修复导出_ses_01234567.json`（`--format markdown` 则为
    /// `.md`），目录里附一个 `_index.json`（筛选条件快照 + 每个文件的元数据）。
    /// 默认只导**顶层 / 未归档 / 非空壳**会话；已存在的同名文件默认**跳过**
    /// （方便反复重跑补增量），`--force` 覆盖；单条失败不中断整批。
    /// 先跑 `--dry-run` 看清单。
    ///
    /// 批量模式必须至少给一个筛选条件（或用 `--all` 显式导出全部），
    /// 避免一条手滑命令把整个库写进当前目录。
    Export {
        /// 会话 id。给了就是单会话导出；不给则进入批量模式。
        session_id: Option<String>,

        /// 输出路径。单会话模式是**文件**（缺省 `./<id>.<ext>`），
        /// 批量模式是**目录**（必须显式指定，不存在则创建）。
        #[arg(short, long, value_name = "PATH")]
        output: Option<PathBuf>,

        /// 导出格式：`json`（原始记录的无损镜像，默认）/ `markdown`
        /// （版式化对话，`## User` / `## Assistant` + 工具调用与结果）。
        ///
        /// 批量模式下扩展名随之变成 `.json` / `.md`；`_index.json` 始终是
        /// JSON（它是清单，不是会话正文）。
        #[arg(short = 'f', long, value_enum, default_value_t = ExportFormat::Json)]
        format: ExportFormat,

        /// 覆盖已存在的文件：单会话模式是「允许覆盖」，批量模式是
        /// 「覆盖同名文件」（默认跳过已存在的）。
        ///
        /// 没有短选项：`-f` 留给 `--format`，与其他子命令保持一致。
        #[arg(long)]
        force: bool,

        /// 只打印将要写出的路径，不写盘。
        #[arg(long)]
        dry_run: bool,

        /// 批量模式：允许不筛选、导出该 app 的全部会话。
        #[arg(long)]
        all: bool,

        /// 只导出该项目的会话。**只接受绝对路径，全等匹配** `project.worktree`。
        #[arg(long = "project", value_name = "ABS_PATH")]
        project: Option<String>,

        /// 按项目路径**子串**模糊过滤（多命中是预期行为）。
        ///
        /// 与 `--project` 互斥。
        #[arg(long = "project-like", value_name = "SUBSTR")]
        project_like: Option<String>,

        /// 只导出工作目录包含该子串的会话。
        #[arg(long, value_name = "SUBSTR")]
        directory: Option<String>,

        /// 只导出这些 agent，可重复（如 build / plan）。
        #[arg(short, long = "agent", value_name = "NAME")]
        agents: Vec<String>,

        /// 只导出标题包含该子串的会话（大小写不敏感）。
        #[arg(short = 't', long = "title", value_name = "SUBSTR")]
        title: Option<String>,

        /// 只导出最近 N 分钟内更新过的会话。
        #[arg(long, value_name = "MINUTES")]
        since: Option<i64>,

        /// 子 agent 会话（opencode 派出的探查任务）的纳入方式。
        #[arg(long, value_enum, default_value_t = SubArg::Exclude, value_name = "MODE")]
        subagents: SubArg,

        /// 归档会话的纳入方式。
        #[arg(long, value_enum, default_value_t = ArchArg::Exclude, value_name = "MODE")]
        archived: ArchArg,

        /// 空壳会话的纳入方式。
        #[arg(long, value_enum, default_value_t = EmptyArg::Exclude, value_name = "MODE")]
        empty: EmptyArg,

        /// 批量模式：最多导出多少条（缺省 = 不限制；0 = 一条都不要）。
        #[arg(short, long, value_name = "N")]
        limit: Option<usize>,

        /// 批量模式：排序键（缺省按最后更新时间倒序，同时决定
        /// `_index.json` 的顺序）。
        #[arg(long, value_enum, value_name = "KEY")]
        sort: Option<SortArg>,
    },

    /// 查看该 app 的登录态（账号 + 凭据有效期）。
    ///
    /// token / api key 默认脱敏输出（只留前缀和长度）。`--show-secrets`
    /// 才会打印完整凭据 —— 仅在排查认证问题时用，别把输出粘到 issue 里。
    Auth {
        /// 输出格式：`table`（键值树，给人扫）/ `json` / `toon`。
        #[arg(long, short = 'f', value_enum, default_value_t = Format::Table)]
        format: Format,

        /// 打印完整 token / api key（默认脱敏）。输出可能被管道或剪贴板带走。
        #[arg(long, short = 's')]
        show_secrets: bool,
    },

    /// 覆盖导入一个本工具 `export` 产出的文件（同 `session_id` 整行替换）。
    ///
    /// 默认**只预检**（打印将覆盖的会话、目标路径，不写库）；加 `--apply`
    /// 才真正写库，且写前自动备份整个数据库。目前仅 zed 支持。
    Import {
        /// 本工具 `export` 产出的 JSON 文件。
        file: PathBuf,

        /// 真正写库（默认只预检）。写前自动备份。
        #[arg(long)]
        apply: bool,
    },
}

/// 要操作的 agent 应用。
///
/// 目前实现了 opencode / codebuddy / codebuddy-ide / zed / claude；将来加 Codex 时在这里加变体即可。
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum AppArg {
    /// opencode（`~/.local/share/opencode/opencode.db`）。
    Opencode,
    /// codebuddy CLI（`~/.codebuddy/projects/<编码路径>/*.jsonl`）。
    Codebuddy,
    /// codebuddy IDE（`%LOCALAPPDATA%\CodeBuddyExtension\Data`，只读）。
    CodebuddyIde,
    /// claude（`~/.claude/projects/<编码路径>/*.jsonl`）。
    Claude,
    /// zed（`~/.local/share/zed/threads/threads.db`）。
    Zed,
}

impl AppArg {
    /// 该 app 的默认数据路径。
    ///
    /// opencode 是数据库文件，codebuddy 是目录（`~/.codebuddy`）。
    ///
    /// 家目录取不到时**直接报错**，不退回空路径：空路径会拼出
    /// `.local/share/opencode/opencode.db` 这种相对路径，把「不知道家目录在
    /// 哪」伪装成「库不存在」，排查成本极高（Windows 上没有 `HOME` 时正是如此）。
    fn default_data_path(self) -> Result<PathBuf> {
        let home = aa_session_core::home_dir()
            .ok_or_else(|| anyhow::anyhow!("无法定位用户家目录（HOME / USERPROFILE 均未设置）"))?;
        Ok(match self {
            AppArg::Opencode => home
                .join(aa_session_source_opencode::DEFAULT_DATA_DIR)
                .join(aa_session_source_opencode::DB_FILE_NAME),
            AppArg::Codebuddy => home.join(aa_session_source_codebuddy::DEFAULT_DATA_DIR),
            AppArg::CodebuddyIde => aa_session_source_codebuddy_ide::default_data_root()
                .ok_or_else(|| {
                    anyhow::anyhow!("无法定位 CodeBuddy IDE 数据目录（LOCALAPPDATA 未设置）")
                })?,
            AppArg::Claude => home.join(aa_session_source_claude::DEFAULT_DATA_DIR),
            AppArg::Zed => home.join(aa_session_source_zed::DEFAULT_DATA_DIR),
        })
    }

    /// `--db` 有值就用它，否则回落到该 app 的默认路径。
    fn resolve_path(self, db: Option<&Path>) -> Result<PathBuf> {
        match db {
            Some(path) => Ok(path.to_path_buf()),
            None => self.default_data_path(),
        }
    }

    /// 按 `--app` 分派到对应的数据源实现。
    fn open(self, db: Option<&std::path::Path>) -> Result<Box<dyn SessionSource>> {
        match self {
            AppArg::Opencode => {
                let path = self.resolve_path(db)?;
                Ok(Box::new(OpencodeSource::open(&path).with_context(
                    || {
                        format!(
                            "无法打开 {} 的数据库（路径 {}）",
                            self.name(),
                            path.display()
                        )
                    },
                )?))
            }
            AppArg::Codebuddy => {
                let path = self.resolve_path(db)?;
                Ok(Box::new(CodebuddySource::open_dir(&path).with_context(
                    || {
                        format!(
                            "无法打开 {} 的数据目录（路径 {}）",
                            self.name(),
                            path.display()
                        )
                    },
                )?))
            }
            AppArg::CodebuddyIde => {
                let path = self.resolve_path(db)?;
                Ok(Box::new(CodebuddyIdeSource::open_dir(&path).with_context(
                    || {
                        format!(
                            "无法打开 {} 的数据目录（路径 {}）",
                            self.name(),
                            path.display()
                        )
                    },
                )?))
            }
            AppArg::Claude => {
                let path = self.resolve_path(db)?;
                Ok(Box::new(ClaudeSource::open_dir(&path).with_context(
                    || {
                        format!(
                            "无法打开 {} 的数据目录（路径 {}）",
                            self.name(),
                            path.display()
                        )
                    },
                )?))
            }
            AppArg::Zed => {
                // `--db` 可给 zed 数据目录（`~/.local/share/zed`，内部拼
                // `threads/threads.db`）或直接给 .db 文件路径。
                let path = self.resolve_path(db)?;
                let db_file = if path.is_file() {
                    path.clone()
                } else {
                    path.join(aa_session_source_zed::DB_FILE_NAME)
                };
                Ok(Box::new(ZedSource::open(&db_file).with_context(|| {
                    format!(
                        "无法打开 {} 的数据库（路径 {}）",
                        self.name(),
                        db_file.display()
                    )
                })?))
            }
        }
    }

    /// 该 app 的显示名。
    fn name(self) -> &'static str {
        match self {
            AppArg::Opencode => "opencode",
            AppArg::Codebuddy => "codebuddy",
            AppArg::CodebuddyIde => "codebuddy-ide",
            AppArg::Claude => "claude",
            AppArg::Zed => "zed",
        }
    }

    /// 打开该 app 的**导入器**（写路径）。
    ///
    /// 目前只有 zed 实现了覆盖导入（`SessionImporter`）。其他 app 返回
    /// `None`，CLI 层报「该 app 暂不支持导入」。写路径和读路径分开构造，
    /// 保证「导入命令一定用写连接」在类型层面成立。
    fn open_importer(&self, db: Option<&Path>) -> Option<Box<dyn SessionImporter>> {
        match self {
            AppArg::Zed => {
                // `--db` 可给 zed 数据目录（`~/.local/share/zed`，内部拼
                // `threads/threads.db`）或直接给 .db 文件路径。
                let path = self.resolve_path(db).ok()?;
                let db_file = if path.is_file() {
                    path
                } else {
                    path.join(aa_session_source_zed::DB_FILE_NAME)
                };
                let src = ZedSource::open_writable(&db_file).ok()?;
                Some(Box::new(src))
            }
            _ => None,
        }
    }
}

/// 归档会话的纳入方式（对应 `aa_session_core::Archived`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ArchArg {
    /// 排除归档会话（默认）。
    Exclude,
    /// 只显示归档会话。
    Only,
    /// 归档与未归档都显示。
    Include,
}

impl From<ArchArg> for Archived {
    fn from(v: ArchArg) -> Self {
        match v {
            ArchArg::Exclude => Archived::Exclude,
            ArchArg::Only => Archived::Only,
            ArchArg::Include => Archived::Include,
        }
    }
}

/// 空壳会话（0 消息）的纳入方式（对应 `aa_session_core::EmptySessions`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum EmptyArg {
    /// 排除空壳会话（默认）—— opencode 的 UI 也不显示它们。
    Exclude,
    /// 只显示空壳会话。
    Only,
    /// 空壳与正常会话都显示。
    Include,
}

impl From<EmptyArg> for EmptySessions {
    fn from(v: EmptyArg) -> Self {
        match v {
            EmptyArg::Exclude => EmptySessions::Exclude,
            EmptyArg::Only => EmptySessions::Only,
            EmptyArg::Include => EmptySessions::Include,
        }
    }
}

/// 子 agent 会话的纳入方式（对应 `aa_session_core::Subagents`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum SubArg {
    /// 只显示顶层会话（默认）。
    Exclude,
    /// 顶层与子会话都显示。
    Include,
    /// 只显示子会话。
    Only,
}

impl From<SubArg> for Subagents {
    fn from(v: SubArg) -> Self {
        match v {
            SubArg::Exclude => Subagents::Exclude,
            SubArg::Include => Subagents::Include,
            SubArg::Only => Subagents::Only,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum SortArg {
    /// 按最后更新时间倒序（默认）。
    Updated,
    /// 按创建时间倒序。
    Created,
    /// 按标题排序。
    Title,
    /// 按总 token 数倒序。
    Tokens,
}

impl From<SortArg> for SortKey {
    fn from(v: SortArg) -> Self {
        match v {
            SortArg::Updated => SortKey::UpdatedDesc,
            SortArg::Created => SortKey::CreatedDesc,
            SortArg::Title => SortKey::Title,
            SortArg::Tokens => SortKey::Tokens,
        }
    }
}

fn main() -> Result<()> {
    let cli = Args::parse();

    // `--app` 必填。手动校验而不是交给 clap，是为了让报错告诉用户有哪些选项，
    // 而不是干巴巴一句 "required arguments were not provided"。
    let app = cli.app.ok_or_else(|| {
        anyhow::anyhow!(
            "必须用 --app 指定要操作的 agent 应用。\n\
             可用：{}",
            AppArg::value_variants()
                .iter()
                .filter_map(|v| v.to_possible_value().map(|p| p.get_name().to_string()))
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;

    let filter = match &cli.log {
        Some(spec) => EnvFilter::new(spec),
        None => EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();

    let source = app.open(cli.db.as_deref())?;

    match cli.command {
        Command::Projects {
            min_sessions,
            format,
            json,
        } => {
            let projects = source.projects()?;
            let projects: Vec<_> = projects
                .into_iter()
                .filter(|p| min_sessions.is_none_or(|n| p.session_count >= n))
                .collect();
            output::print_projects(&projects, resolve_format(format, json))?;
        }

        Command::RenameProject {
            old_path,
            new_path,
            apply,
            allow_missing,
            backup_to,
        } => {
            run_rename_project(
                app,
                cli.db.as_ref(),
                &old_path,
                &new_path,
                apply,
                allow_missing,
                backup_to,
            )?;
        }

        Command::Directories {
            directory,
            min_sessions,
            format,
            json,
        } => {
            let dirs = source.directories()?;
            let needle = directory.map(|d| d.to_lowercase());
            let dirs: Vec<_> = dirs
                .into_iter()
                .filter(|d| min_sessions.is_none_or(|n| d.session_count >= n))
                .filter(|d| {
                    needle
                        .as_ref()
                        .is_none_or(|n| d.path.to_lowercase().contains(n))
                })
                .collect();
            output::print_directories(&dirs, resolve_format(format, json))?;
        }

        Command::Sessions {
            limit,
            project,
            project_like,
            directory,
            agents,
            title,
            since,
            subagents,
            archived,
            empty,
            sort,
            format,
            json,
        } => {
            let query = QueryArgs {
                project,
                project_like,
                directory,
                agents,
                title,
                since,
                subagents,
                archived,
                empty,
                sort,
            };
            let sessions = query_sessions(source.as_ref(), &query, limit)?;
            output::print_sessions(&sessions, resolve_format(format, json))?;
        }

        Command::Show { session_id, format } => {
            let text = match format {
                ExportFormat::Json => {
                    let file = assemble(source.as_ref(), &session_id)?;
                    serde_json::to_string_pretty(&file)?
                }
                ExportFormat::Markdown => source.markdown(&session_id)?,
            };
            out(format_args!("{text}"));
        }

        Command::Export {
            session_id,
            output,
            format,
            force,
            dry_run,
            all,
            project,
            project_like,
            directory,
            agents,
            title,
            since,
            subagents,
            archived,
            empty,
            limit,
            sort,
        } => match session_id {
            // ---- 单会话 ----
            Some(id) => {
                // 筛选参数在单会话模式下没有意义。静默忽略会让用户以为
                // `--title x` 生效了，不如直接拒绝。
                let ignored = [
                    ("--all", all),
                    ("--project", project.is_some()),
                    ("--project-like", project_like.is_some()),
                    ("--directory", directory.is_some()),
                    ("--agent", !agents.is_empty()),
                    ("--title", title.is_some()),
                    ("--since", since.is_some()),
                    ("--limit", limit.is_some()),
                    ("--sort", sort.is_some()),
                    ("--subagents", subagents != SubArg::Exclude),
                    ("--archived", archived != ArchArg::Exclude),
                    ("--empty", empty != EmptyArg::Exclude),
                ];
                let ignored: Vec<&str> = ignored
                    .iter()
                    .filter(|(_, provided)| *provided)
                    .map(|(flag, _)| *flag)
                    .collect();
                if !ignored.is_empty() {
                    anyhow::bail!(
                        "单会话导出不接受这些参数：{}\n\
                         要按条件批量导出请省略 SESSION_ID。",
                        ignored.join(", ")
                    );
                }
                export::write_single(source.as_ref(), &id, output, force, dry_run, format)?;
            }

            // ---- 批量 ----
            None => {
                let has_filter = project.is_some()
                    || project_like.is_some()
                    || directory.is_some()
                    || title.is_some()
                    || since.is_some()
                    || !agents.is_empty();

                // `--subagents` / `--archived` / `--empty` 是「放宽范围」的开关，
                // 不算筛选条件 —— 光给它们仍然等于「整个库」。
                if all && has_filter {
                    anyhow::bail!("--all 与筛选条件互斥：要么导出全部，要么给出筛选条件。");
                }
                if !all && !has_filter {
                    anyhow::bail!(
                        "批量导出需要至少一个筛选条件（--project / --project-like / \
                         --directory / --agent / --title / --since），\n\
                         或者加 --all 导出该 app 的全部会话。\n\
                         只想导出一个会话就直接写 SESSION_ID。"
                    );
                }

                let Some(out_dir) = output else {
                    anyhow::bail!(
                        "批量导出需要 -o 指定输出目录，例如 -o exports/LocalDub/opencode"
                    );
                };

                let query = QueryArgs {
                    project,
                    project_like,
                    directory,
                    agents,
                    title,
                    since,
                    subagents,
                    archived,
                    empty,
                    sort: sort.unwrap_or(SortArg::Updated),
                };
                // 缺省 = 不限制。用 `Option` 而不是「0 表示不限制」：这样
                // `--limit 0` 仍然是字面意义（一条都不要），不会出现
                // 「想试 0 条结果导出了全部」这种反直觉行为。
                let limit = limit.unwrap_or(NO_LIMIT);
                let sessions = query_sessions(source.as_ref(), &query, limit)?;

                export::run_batch(
                    source.as_ref(),
                    &sessions,
                    &export::BatchOptions {
                        out_dir,
                        force,
                        dry_run,
                        format,
                    },
                    query.describe(),
                )?;
            }
        },

        Command::Auth {
            format,
            show_secrets,
        } => {
            let info = source.auth(show_secrets)?;
            output::print_auth(&info, format, show_secrets)?;
        }

        Command::Import { file, apply } => {
            let Some(mut importer) = app.open_importer(cli.db.as_deref()) else {
                anyhow::bail!(
                    "app `{}` 暂不支持导入（目前只有 zed 实现了覆盖导入）。",
                    app.name()
                );
            };
            let report = importer.import(&file, apply)?;
            out(format_args!("{}", report.summary));
            if report.backup.is_some() {
                out(format_args!(
                    "备份：{}",
                    report.backup.as_deref().unwrap_or("")
                ));
            }
        }
    }

    Ok(())
}

/// 把 `--project` 的输入规范成一个**库中真实存在的绝对路径**。
///
/// 三条规则，都是为了防错：
///
/// 1. **必须是绝对路径**。相对路径一律拒绝并给出建议 —— 库里的 worktree
///    全是绝对路径（实测 45/45），相对路径必然匹配不上。
/// 2. **不做 `canonicalize()`**。实测 12 个项目的目录已从磁盘删除，其下仍有
///    **244 条会话**；`canonicalize` 会因为路径不存在而失败，让这批会话永远
///    查不到。只做去尾部斜杠 + 展开 `~` 这类「字符串层面」的规范化。
/// 3. **尾部 `/` 去掉**。`/a/b/` 和 `/a/b` 在库里是同一个项目。
fn resolve_exact_project(source: &dyn SessionSource, input: &str) -> Result<String> {
    // `~` 展开：用户在 shell 里可能没引号，`~/foo` 会被 shell 自己展开，
    // 但 `--project '~/foo'` 传到我们这儿还是带波浪号的。
    let mut path = if let Some(rest) = input.strip_prefix("~/") {
        std::env::var("HOME")
            .map(|h| PathBuf::from(h).join(rest).display().to_string())
            .unwrap_or_else(|_| input.to_string())
    } else {
        input.to_string()
    };

    // 去掉尾部斜杠，但保留根目录 "/" 本身。
    while path.len() > 1 && path.ends_with('/') {
        path.pop();
    }

    if !path.starts_with('/') {
        anyhow::bail!(
            "--project 只接受绝对路径，收到的是 `{input}`。\n\
             模糊匹配请用 --project-like {input}"
        );
    }

    // `global` 兜底项目的 worktree 就是 "/"，命中它要提醒 —— 用户多半是想查
    // 某个具体目录，而不是这 24 条散落 6 处的会话。
    if path == aa_session_core::UNKNOWN_PROJECT_PATH {
        // 会话数取自 `projects()` 的口径（顶层 + 未归档），而不是再跑一次
        // sessions —— `SessionFilter::default()` 的 limit 是 0，拿它计数会得到 0。
        let n = source
            .projects()?
            .into_iter()
            .find(|p| p.is_unknown_path())
            .map(|p| p.session_count)
            .unwrap_or(0);
        eprintln!(
            "提示：`/` 是 opencode 对「无法确定所属仓库」的占位值，对应 {n} 条顶层会话，散落在多个目录里。"
        );
        eprintln!("要看具体目录，用 `aa-session --app opencode directories`。");
    }

    // 存在性检查放在解析之后 —— 用 `projects()` 校验而不是额外的 SQL。
    // 同路径多 id 会返回多行，那正是要聚合的情况，不算错。
    let known = source
        .projects()?
        .into_iter()
        .any(|p| normalize_path(&p.path) == path);
    if !known {
        anyhow::bail!(
            "库中没有 worktree 为 `{path}` 的项目。\n\
                        先跑 `aa-session --app opencode projects` 看看有哪些。"
        );
    }

    Ok(path)
}

/// 路径字符串层面的规范化（与 `resolve_exact_project` 保持一致）。
fn normalize_path(path: &str) -> String {
    let mut p = path.to_string();
    while p.len() > 1 && p.ends_with('/') {
        p.pop();
    }
    p
}

/// 「N 分钟前」的 Unix 毫秒时间戳。
fn minutes_ago_ms(minutes: i64) -> i64 {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    now_ms - minutes * 60 * 1000
}

/// `sessions` 与 `export-many` 共用的筛选条件。
///
/// 两个子命令的筛选参数完全一致，抽出来是为了让「批量导出哪些会话」和
/// 「列表里看到哪些会话」在实现层面就是同一段代码 —— 否则两边迟早走偏
/// （比如一边修了 `--project` 的边界处理，另一边忘了）。
struct QueryArgs {
    project: Option<String>,
    project_like: Option<String>,
    directory: Option<String>,
    agents: Vec<String>,
    title: Option<String>,
    since: Option<i64>,
    subagents: SubArg,
    archived: ArchArg,
    empty: EmptyArg,
    sort: SortArg,
}

impl QueryArgs {
    /// 筛选条件快照，写进批量导出的 `_index.json`。
    ///
    /// 过几天回来能知道「这个目录是怎么筛出来的」，而不是靠回忆。
    fn describe(&self) -> serde_json::Value {
        serde_json::json!({
            "project": self.project,
            "project_like": self.project_like,
            "directory": self.directory,
            "agents": self.agents,
            "title": self.title,
            "since_minutes": self.since,
            "subagents": mode_name(self.subagents),
            "archived": mode_name(self.archived),
            "empty": mode_name(self.empty),
            "sort": mode_name(self.sort),
        })
    }
}

/// `ValueEnum` 变体的命令行名字（kebab-case），用于输出与索引。
fn mode_name<T: ValueEnum>(value: T) -> String {
    value
        .to_possible_value()
        .map(|p| p.get_name().to_string())
        .unwrap_or_default()
}

/// 按筛选条件查会话。
///
/// 项目寻址的两条规则（精确 / 模糊互斥、命中为空要报错）集中在这里，
/// 保证 `sessions` 和 `export-many` 行为一致。
fn query_sessions(
    source: &dyn SessionSource,
    query: &QueryArgs,
    limit: usize,
) -> Result<Vec<SessionMeta>> {
    if query.project.is_some() && query.project_like.is_some() {
        anyhow::bail!(
            "--project（精确）与 --project-like（模糊）互斥，请只用一个。\n\
             精确匹配要绝对路径；模糊匹配才是子串。"
        );
    }

    let selector = match (&query.project, &query.project_like) {
        (Some(p), _) => ProjectSelector::Exact {
            path: resolve_exact_project(source, p)?,
        },
        (_, Some(pattern)) => ProjectSelector::Like {
            pattern: pattern.clone(),
            paths: source.project_paths_like(pattern)?,
        },
        _ => ProjectSelector::All,
    };

    if let ProjectSelector::Like { paths, pattern } = &selector
        && paths.is_empty()
    {
        // 空结果必须提示，否则用户会以为是「没有会话」而不是「没有项目」。
        anyhow::bail!(
            "没有路径包含 `{pattern}` 的项目；\
             先跑 `aa-session --app opencode projects` 看看有哪些。"
        );
    }

    let filter = SessionFilter {
        project: selector,
        directory: query.directory.clone(),
        agents: query.agents.clone(),
        title_contains: query.title.clone(),
        // 「最近 N 分钟」在 SQL 里比较，不把全表拉进内存。
        updated_after: query.since.map(minutes_ago_ms),
        subagents: query.subagents.into(),
        archived: query.archived.into(),
        empty: query.empty.into(),
        limit,
    };

    Ok(source.sessions(&filter, query.sort.into())?)
}

/// 改名子命令的实现。
///
/// 安全设计（这是本工具唯一写库的路径，规则必须硬）：
///
/// 1. **默认只预检**。没有 `--apply` 就只打印将要发生的改动。
/// 2. **写前强制备份**，文件名带时间戳，每次都是新的撤销点、不会覆盖上一次。
/// 3. **磁盘存在性校验**，除非 `--allow-missing`。
/// 4. 目标路径**不能被其他项目占用**。
#[allow(clippy::too_many_arguments)]
fn run_rename_project(
    app: AppArg,
    db_override: Option<&PathBuf>,
    old_path: &str,
    new_path: &str,
    apply: bool,
    allow_missing: bool,
    backup_to: Option<PathBuf>,
) -> Result<()> {
    // 复用精确寻址的规范化（去尾部 /、展开 ~），保证和查询时同一套规则。
    let old_path = normalize_cli_path(old_path)?;
    let new_path = normalize_cli_path(new_path)?;

    if old_path == new_path {
        anyhow::bail!("新旧路径相同，无需改名");
    }

    for (label, p) in [("旧", &old_path), ("新", &new_path)] {
        if !p.starts_with('/') {
            anyhow::bail!("{label}路径 `{p}` 不是绝对路径");
        }
    }

    // 跟随 `--db`：用户显式给了路径就是想操作那个库，而且这样才能在副本上
    // 做写入演练。默认值仍是 app 的标准位置。
    let db_path = match db_override {
        Some(path) => path.clone(),
        None => app.default_data_path()?,
    };

    // 预检用只读连接。
    let ro = OpencodeSource::open(&db_path)?;
    let old_ids = ro.project_ids_for_path(&old_path)?;
    if old_ids.is_empty() {
        anyhow::bail!(
            "库中没有 worktree 为 `{old_path}` 的项目；\
             先跑 `aa-session --app {} projects` 看看有哪些。",
            app.name()
        );
    }

    // 目标不能被别的项目占用（同路径多个 id 是历史遗留，不算冲突）。
    let new_ids = ro.project_ids_for_path(&new_path)?;
    if !new_ids.is_empty() {
        anyhow::bail!(
            "路径 `{new_path}` 已被 {} 个项目占用，拒绝改名以免合并到一起",
            new_ids.len()
        );
    }

    // 磁盘存在性：新路径必须真的在那儿，否则多半是打错字。
    if !allow_missing && !std::path::Path::new(&new_path).exists() {
        anyhow::bail!(
            "新路径 `{new_path}` 在磁盘上不存在。\n\
             如果这是预期的（例如目录还没建、或者它在另一台机器上），加 --allow-missing。"
        );
    }

    let plan = rename::plan(ro.conn(), &old_path, &new_path)?;

    out(format_args!("项目改名预检  {} → {}", old_path, new_path));
    out(format_args!(
        "  project.worktree          {} 行",
        plan.project_rows
    ));
    out(format_args!(
        "  session_v2.directory       {} 行",
        plan.session_v2_rows
    ));
    out(format_args!(
        "  session.directory (v1)    {} 行",
        plan.session_rows
    ));
    out(format_args!(
        "  消息历史内的路径           0 行（刻意不改，见 AGENTS.md）"
    ));
    out(format_args!(
        "  合计改动                   {} 行",
        plan.total()
    ));

    if plan.is_noop() {
        out(format_args!("\n没有任何记录需要改动。"));
        return Ok(());
    }

    if !apply {
        out(format_args!("\n这是预检。确认无误后加 --apply 执行。"));
        return Ok(());
    }

    // ---- 以下才真正写库 ----
    eprintln!("\n⚠ 即将写入 opencode 数据库：{}", db_path.display());
    eprintln!("⚠ 请确认 opencode 没有在运行，否则它内存里的会话状态会与磁盘不一致。");

    let dest = backup_to.unwrap_or_else(|| backup::backup_path_for(&db_path));
    let mut rw = OpencodeSource::open_writable(&db_path)?;

    let saved = backup::backup(rw.conn(), &dest)?;
    let size = std::fs::metadata(&saved).map(|m| m.len()).unwrap_or(0);
    out(format_args!(
        "\n已备份 → {}（{}）",
        saved.display(),
        human_bytes(size)
    ));

    let applied = rename::apply(rw.conn_mut(), &old_path, &new_path)?;
    out(format_args!(
        "已改名：{} 行。备份保留在 {}",
        applied.total(),
        saved.display()
    ));
    out(format_args!(
        "撤销方式：用备份文件替换主库（先关闭 opencode）。"
    ));

    Ok(())
}

/// `--json` 是 `--format json` 的简写；两者同时给，`--json` 优先。
fn resolve_format(format: Format, json_flag: bool) -> Format {
    if json_flag { Format::Json } else { format }
}

/// `--project` / `rename-project` 共用的路径规范化。
fn normalize_cli_path(input: &str) -> Result<String> {
    let mut path = if let Some(rest) = input.strip_prefix("~/") {
        std::env::var("HOME")
            .map(|h| PathBuf::from(h).join(rest).display().to_string())
            .unwrap_or_else(|_| input.to_string())
    } else {
        input.to_string()
    };
    while path.len() > 1 && path.ends_with('/') {
        path.pop();
    }
    Ok(path)
}
