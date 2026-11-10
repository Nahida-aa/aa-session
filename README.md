# aa-session

列出并导出本地 agent 会话（opencode / codebuddy / codebuddy-ide / claude / zed），导成**可以直接在编辑器里全文搜索**的 JSON 或 Markdown。

## 为什么做这个

核心判断：**全文检索交给用户自己的编辑器**，这里只负责把会话导出成可读文本。

Zed 里一个 `cmd+f` 就能跨文件搜「音频解码」，命中的就是会话原文。自己维护索引
（曾经有过 SQLite FTS5 侧车）的代价是 470 MB 重复数据、永远可能过期、还要管
schema 版本——而列举 891 个会话本来就只是一次 SQL 查询。

所以导出物**不做格式归一**：每条消息一个对象，`text` 字段是裸文本。

## 安装

```bash
cargo build --release
# 二进制在 target/release/aa-session
```

需要 Rust 2024 edition（`rustc` ≥ 1.85）。SQLite 用 `rusqlite` 的 `bundled` 特性，
不需要系统装 sqlite。

## 快速开始

`--app` **必填**，放在子命令前后都行：

```bash
# 1. 有哪些项目
aa-session --app opencode projects

# 2. 某个项目下有哪些会话
aa-session --app opencode sessions --project-like LocalDub

# 3. 导出一个会话
aa-session --app opencode export ses_xxx -o chat.json

# 4. 导出整个目录的会话（一会话一文件 + _index.json）
aa-session --app opencode export --directory LocalDub -o exports/LocalDub/opencode

# 5. 在编辑器里搜
zed exports/LocalDub/opencode
```

`projects` 的输出（`会话` / `子会话` / `归档` 三列各按自己的口径统计，互不重叠）：

```
 会话  子会话  归档  最近活跃          项目路径
 2     0       0     2026-11-02 00:49  /home/aa/repos/ai_ls/vision-lab
 2     0       0     2026-11-02 00:22  /home/aa/repos/ide_ls/aa-session
```

`sessions` 的输出（路径从左侧截断，`同左` 表示会话目录即项目目录）：

```
 会话 ID        时间                tokens  agent  成本  标题              会话目录                          项目目录
 ses_04618623…  2026-11-02 00:23    74.9万  build     -  subtitle-ocr      /home/aa/repos/ai_ls/vision-lab   同左
```

## 支持的 app

| `--app` | 存储 | 读 | 写 |
| --- | --- | --- | --- |
| `opencode` | `~/.local/share/opencode/opencode.db`（SQLite，约 6 GB） | ✅ | `rename-project` |
| `codebuddy` | `~/.codebuddy/projects/<编码路径>/*.jsonl` | ✅ | ❌ |
| `codebuddy-ide` | `%LOCALAPPDATA%\CodeBuddyExtension\Data\<窗口>\CodeBuddyIDE\...`（三层 JSON 树） | ✅ | ❌ |
| `claude` | `~/.claude/projects/<编码路径>/<uuid>.jsonl` | ✅ | ❌ |
| `zed` | `~/.local/share/zed/threads/threads.db`（zstd 压缩 blob） | ✅ | `import`（覆盖导入） |

`opencode` 的 `rename-project` 只改「指针」字段（`project.worktree`、`session.directory`），
**不改**消息历史里的路径——那是当时实际发生的事。

各自直读自己的存储，**不共享代码**。`--db` 可覆盖默认路径。

> ⚠️ codebuddy 会按 `cleanupPeriodDays`（默认 **30 天**）清理本地会话，
> 过期连文件一起消失。要长期归档必须导出，见
> [`docs/codebuddy-retention.md`](docs/codebuddy-retention.md)。

## 命令

```
aa-session --app <APP> projects  [--min-sessions N] [--format table|json|toon]
aa-session --app <APP> directories [--directory SUB] [--min-sessions N] [--format ...]
aa-session --app <APP> sessions  [过滤 + 排序 + 格式，见下]
aa-session --app <APP> show    <SESSION_ID> [--format json|markdown]
aa-session --app <APP> export  [SESSION_ID] [-o PATH] [--format json|markdown]
                               [--force] [--dry-run] [--all] [过滤 + 排序]
aa-session --app <APP> auth    [--show-secrets]
aa-session --app <APP> import  <FILE> [--apply]
aa-session --app <APP> rename-project <OLD_PATH> <NEW_PATH> [--apply]
```

### 过滤与排序（`sessions` / `export` 共用）

| 参数 | 语义 |
| --- | --- |
| `--project <ABS_PATH>` | **全等**匹配项目路径，只收绝对路径；自动聚合同路径的多个 project id |
| `--project-like <SUB>` | 项目路径**子串**匹配，多命中正常全列 |
| `--directory <SUB>` | 会话**运行目录**子串（不是项目归属，两者可以不同） |
| `--agent <NAME>` | 按 agent 名过滤，可重复 |
| `--title <SUB>` | 标题子串 |
| `--since <MINUTES>` | 最近 N 分钟内更新过 |
| `--subagents exclude\|include\|only` | 子 agent 会话，默认 `exclude` |
| `--archived exclude\|only\|include` | 归档会话，默认 `exclude` |
| `--sort updated\|created\|title\|tokens` | 排序，默认 updated |
| `--limit N` | 缺省不限；`--limit 0` 表示一条都不要 |

`--project` 不做 `canonicalize()`——很多项目目录已经从磁盘删掉，但会话还在，
canonicalize 会让这批会话永远查不到。只做去尾斜杠、展开 `~` 这类字符串规范化。

### 默认只看顶层 + 未归档

opencode 会把主 agent 派出的探查任务记成独立会话。本机 891 个会话里 **652 个是
子会话**（607 个是 `explore`），标题雷同、目录相同，默认隐藏；归档同理。

### `export`：单个与批量是同一条命令

- 给 `SESSION_ID` → 导一个文件（此时给筛选参数会直接报错，不做静默忽略）。
- 不给 → 按筛选条件批量导出到目录，一会话一文件 + `_index.json`。
  **必须至少给一个筛选条件或 `--all`**，防止手滑把整个库（几十 GB）写进当前目录。

批量模式的文件名是 `<创建日期>_<标题 slug>_<id 前 12 位>.<ext>`；默认**跳过已存在
文件**所以能反复重跑补增量（`--force` 覆盖）。单条失败不中断整批，最后以非零
退出码报告失败数。`--dry-run` 只打印清单不写盘。

## 导出格式

| `--format` | 内容 | 给谁 |
| --- | --- | --- |
| `json`（默认） | 原始记录的无损镜像，含工具调用参数与结果原文 | 编辑器全文搜索、`import` |
| `markdown` | 版式化对话（`## User` / `## Assistant` + `<thinking>` / 工具调用的代码块） | 人读 |

两种都可能几十 MB（主要是文件全文和命令输出），批量导出前先 `--dry-run`。

单会话 JSON 的形状：

```json
{
  "app": "opencode",
  "session_id": "ses_...",
  "raw_data": { "session": {...}, "session_message": [...], ... }
}
```

`raw_data` 是各 source 的 `dump()` 原样结果，所以不同 app 的字段不同。
`_index.json` 描述本次运行的结果（筛选条件快照 + id / 标题 / 路径 / 字节数），
每次重写，别手改。

## 输出格式

| 格式 | 解决什么 |
| --- | --- |
| `table`（默认） | 视觉对齐，纵向扫一列对比数字最省力。CJK 标题用 `comfy-table` 按显示宽度对齐 |
| `toon` | 省 token：表头只写一次键名，行内只写值（30 条会话实测比 JSON 省 48%） |
| `json` | 管道（`jq`）、脚本。`--json` 是它的简写 |

## justfile

`just` 列出全部配方：

```bash
just projects [过滤]              # 列项目
just sessions [过滤]              # 列会话
just dump <ID> [路径] [格式]      # 导一个会话，默认落到 exports/<ID>.<ext>
just dump-dir <目录子串> [输出目录] [格式]   # 按目录批量导出
just zed <ID>                     # 导出后直接 Zed 打开
just find <模式>                  # 在 exports/ 里全文搜索
just ci                           # fmt + clippy + test，提交前跑这个
```

`exports/` 已 gitignore（单个导出可达数百 MB）。要换目录：
`AA_SESSION_EXPORTS=/path just find <模式>`。

维护 justfile 时的两处陷阱写在 [`AGENTS.md`](AGENTS.md) 的 justfile 一节。

## 写操作与安全边界

- **opencode 库绝不做无备份写入**。该库常被运行中的 opencode 进程持有，写入会与其
  内存状态冲突。`rename-project` 和 `import` 都**默认只预检**，加 `--apply` 才写，
  且写前强制 `VACUUM INTO` 备份（文件名带时间戳 + 单调后缀防撞名）。
- opencode 的读路径以 SQLite `mode=ro` 打开。
- 唯一的**跨 app** 写入是 zed 的 `import`（覆盖导入同 `session_id` 的线程）；
  codebuddy 与 opencode 的 `open_importer()` 都返回 `None`，CLI 会报「暂不支持导入」。

## 架构

```
packages/core/                领域模型 + SessionSource trait（零外部依赖）
packages/sources/opencode/    直读 opencode.db（rusqlite）
packages/sources/codebuddy/   直读 ~/.codebuddy/projects/**/*.jsonl
packages/sources/codebuddy-ide/ 直读 CodeBuddy IDE 的 history/**/index.json + messages/*.json
packages/sources/zed/         直读 threads.db + 覆盖导入
packages/apps/cli/            clap 入口
```

依赖方向单向：`core` ← `sources/*` ← `apps/cli`。`core` 刻意不依赖 `rusqlite` /
`anyhow`——错误只带 `String` 描述，库层用 `thiserror`，只有二进制入口用 `anyhow`。

新增数据源只需再写一个 `SessionSource` 实现；JSONL 形态的照抄 `sources/codebuddy` 最快。

## 开发

```bash
cargo test --workspace
cargo clippy --workspace --all-targets
just ci          # fmt + clippy + test，提交前跑这个
```

更多设计取舍（为什么不做搜索、三个路径字段的区别、TOON 的依赖坑、justfile 的
两处陷阱等）见 [`AGENTS.md`](AGENTS.md)，路线图见 [`ROADMAP.md`](ROADMAP.md)。
