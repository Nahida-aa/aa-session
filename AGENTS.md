# aa-session

列出并导出本地 agent 会话。**用法看 [README](README.md)，取舍与陷阱看本文与 `.agents/`。**

## 先读哪一篇

| 想知道 | 去哪 |
| --- | --- |
| 怎么装、怎么用、有哪些参数、导出长什么样 | [`README.md`](README.md) |
| 改 `sessions` / `projects` / `directories` 的视图与筛选 | [`.agents/cli-design.md`](.agents/cli-design.md) |
| 改 `--format json\|markdown` 的渲染 | [`.agents/export-format.md`](.agents/export-format.md) |
| 动写操作，或要碰 zed / AAgent 的代码 | [`.agents/safety.md`](.agents/safety.md) |
| codebuddy 会话为什么凭空少了 | [`docs/codebuddy-retention.md`](docs/codebuddy-retention.md) |
| 下一步做什么 | [`ROADMAP.md`](ROADMAP.md) |
| 通用 Rust 规则 | [`.agents/rust-skill-index.md`](.agents/rust-skill-index.md)（cargo-skill 生成，与本项目无关） |
| zed 工具调用损坏案例与恢复流程 | [`.agents/zed-tool-call-corruption.md`](.agents/zed-tool-call-corruption.md) |

本文只留**为什么这样设计**（不带实现细节的取舍）与**改动时必须知道的不变量**。

## 定位：为什么不做搜索

核心场景已经定了——**全文检索交给用户自己的编辑器**。我们负责把会话导出成
可读 JSON，用户在 Zed 里搜。理由：

- 不必维护索引（曾建过 SQLite FTS5 侧车，470 MB 重复数据、永远可能过期、
  还要管 schema 版本；删掉后 891 个会话的列举是一次 SQL 查询的事）；
- 不必猜用户想要什么样的检索——Zed / IDE 的搜索比我们的 FTS 强得多；
- 导出物是**给人读的**：每条消息一个对象，`text` 字段直接是裸文本，
  Zed 搜「音频解码」能直接命中字符串本身。

代价：找东西要先知道大致在哪个会话（或先导出再搜）。这是明确的取舍。

## 命令

`--app` **必填**（放在子命令前后都行），不猜目标 app。

```bash
aa-session --app opencode projects   [--min-sessions N] [--json]
aa-session --app opencode directories [--directory SUB] [--min-sessions N] [--json]
aa-session --app opencode sessions   [--project ABS_PATH | --project-like SUB]
                                    [--directory SUB] [--agent NAME]...
                                    [--title SUB] [--since MINUTES]
                                    [--subagents exclude|include|only]
                                    [--archived exclude|only|include]
                                    [--sort updated|created|title|tokens] [--limit N]
                                    [--format table|json|toon]
aa-session --app opencode show   <SESSION_ID> [--format json|markdown]
aa-session --app opencode export [SESSION_ID] [-o PATH] [--format json|markdown]
                                    [--force] [--dry-run] [--all]
                                    [过滤选项同 sessions] [--sort ...] [--limit N]
```

- 表格里**不显示来源列**：`--app` 参数已经说明了是哪个 app。
- `--format` 默认 `table`；`--json` 是 `--format json` 的简写（保留旧用法）。

筛选项（`--subagents` / `--archived` / `--sort` / `--limit`）的语义见 README。
设计与踩坑在 [`.agents/cli-design.md`](.agents/cli-design.md)：
为什么 `--project` 不做 `canonicalize()`、为什么路径从左侧截断、`会话目录` 与
`项目目录` 为什么是两回事。

### `export`：一条命令，单个或批量

「导一个」和「导一批」是同一种操作的两种规模，所以是**同一个子命令**，
区别只在于给不给 `SESSION_ID`：

```bash
# 单个 → -o 是文件
aa-session --app opencode export ses_xxx -o chat.json

# 批量 → -o 是目录（不存在则创建）
aa-session --app opencode export --directory LocalDub -o exports/LocalDub/opencode
aa-session --app opencode export --project /home/aa/repos/ai_ls/aa-zed -o exports/aa-zed/opencode
```

单会话模式下给筛选参数会**直接报错**（`--title` 之类在单会话里没有意义，
静默忽略会让人以为筛过）。

批量模式：

- **筛选条件与 `sessions` 完全一致**（`--project` / `--project-like` /
  `--directory` / `--agent` / `--title` / `--since` + 三个 include 模式），
  两者共用 `query_sessions()`，不会一边修了边界、另一边忘了。
- **必须至少给一个筛选条件**，或者显式 `--all`；`--all` 与筛选条件互斥。
  这条护栏挡的是「一条手滑命令把整个库（891 个会话、几十 GB）写进当前目录」。
- 默认仍是**顶层 + 未归档 + 非空壳**；子 agent 的探查会话要显式
  `--subagents include` 才会导出。
- `--limit` **缺省 = 不限制**；`--limit 0` 是字面意义的「一条都不要」
  （用 `Option<usize>` 区分「没给」和「给了 0」，不再需要「0 表示全部」这种
  反直觉约定）。底层用 `core::NO_LIMIT`：`usize::MAX` 转成 `i64` 是 `-1`，
  SQLite 的负数 `LIMIT` 正是不限，`Vec::truncate` 也是无操作。
- `--format json|markdown`（默认 `json`）：决定文件扩展名（`.json` / `.md`）
  与正文格式。Markdown 见下方「导出格式」一节。
- 文件名 `<创建日期>_<标题 slug>_<id 前 12 位>.<ext>`：日期在前可按时间自然
  排序，标题让人在文件树里认出对话，id 段保证不互相覆盖。标题里的空格 /
  斜杠 / emoji 折叠成 `-`；**id 段单独消毒**，不能靠「数据库里的 id 应该
  没怪字符」——`/` 会写到目录之外。
- 附一个 `_index.json`：筛选条件快照 + 每条会话的 id / 标题 / 路径 /
  token / 字节数 / `pre_existing`。它是**本次运行的结果**，每次重写，别手改。
- 默认**跳过**已存在的同名文件，所以能反复重跑补增量；`--force` 才覆盖。
  文件名里的日期取自**创建时间**，会话后续更新不改名，刷新内容必须 `--force`。
- 单条会话失败**不中断整批**，其余照常写入，最后以非零退出码报告失败数。
- `--dry-run` 只打印清单（文件名 + 目标目录），不创建目录也不写盘；
  单会话模式下同样可用，且不会 dump 内容（先校验 id 存在）。

不合并成单个大文件，是因为：编辑器每次命中都落在具体会话上；增量重跑只写
新增；每份导出物本身就是单会话导出格式，将来要 `import` 不用再拆。

### 默认只看「顶层 + 未归档」

opencode 把主 agent 派出的探查任务记成独立会话（`parent_id` 非空），本库 891 个
会话里 652 个是子会话；归档会话同理。两个维度都默认隐藏，用 `--subagents` /
`--archived` 的 `include` / `only` 放出来。各视图计数口径的一致性约束见
[`.agents/cli-design.md`](.agents/cli-design.md)。

## justfile：用法在 README，陷阱在这里

配方表与 `AA_SESSION_EXPORTS` 这类**用法**写在 README（[`README.md` § justfile](README.md)），
本节只留维护 justfile 时必须知道的坑——这些是踩出来的，不写下来会再踩一次。

### justfile 的两个坑

1. **just 只做文本替换，不创建同名 shell 变量。** 想用 shell 的默认值展开
   `${out:-...}`，必须先 `out="{{ out }}"` 赋值；直接写 `${out:-...}` 会**永远取
   默认值**——用户传了输出路径也不生效。同理 `$session_id` 也得先赋值，否则
   `set -u` 下报「未绑定的变量」。
2. **替换发生在 bash 看到脚本之前。** 插值进 shell 的参数一律用 `{{ quote(...) }}`
   转义，否则 `just find 'a$b'` 会被 bash 当变量、`just find '`id`'` 会被当命令
   **执行**。

另外：`export` 作为 recipe 名会让部分编辑器的 justfile 语法插件误报语法错误
（just 自身解析正常），故改名为 `dump`。

## 数据源

五个 app，各自直读自己的存储，互不共享代码：

| `--app` | 存储 | 形态 |
| --- | --- | --- |
| `opencode` | `~/.local/share/opencode/opencode.db`（约 6 GB） | SQLite |
| `codebuddy` | `~/.codebuddy/projects/<编码路径>/*.jsonl` | 每会话一个 JSONL 事件流 |
| `codebuddy-ide` | `%LOCALAPPDATA%\CodeBuddyExtension\Data\<窗口>\CodeBuddyIDE\<引用>\history\<md5>\` | 工作区 / 会话 / 消息三层 JSON 文件 |
| `claude` | `~/.claude/projects/<编码路径>/<uuid>.jsonl` | 每会话一个 JSONL 事件流（同一条消息会拆成多行） |
| `zed` | `~/.local/share/zed/threads/threads.db` | SQLite（zstd 压缩的 `thread_json` blob） |

opencode 的读取细则：

- 打开时探测 `session_v2` 是否存在，决定整库走哪套 schema：
  - 有 → **会话元数据列表是 v1 + v2 合并读**：`sessions()` / `session()` /
    `projects()` 都从 `session_v2`（+ `session_message` / `project`）和 v1 的
    `session` / `message` / `part` 两张表 `UNION ALL`，v1 分支用
    `s.id NOT IN (SELECT id FROM session_v2)` 去重（同 id 以 v2 为准）。见
    `packages/sources/opencode/src/lib.rs` 的 `SESSION_FROM_PROJECT_V1` 与 `sessions()`；
  - 没有（老版本、以及 Windows 构建）→ 只读 v1（`session` / `message` / `part`），
    **任何一句 SQL 都不能提 `session_v2`**，否则直接报 `no such table`。
- dump / Markdown 渲染层相反：同一会话两表都有残留行时**只读 v2**
  （`session_message` 非空即视为 v2），否则对话会打印两遍。见
  [`.agents/export-format.md`](.agents/export-format.md)。
- v1 的 `project` 表没有 `time_active` 列（实测本机就是如此），用 `time_updated`
  作为最近活跃时间的等价替代。
- 以 SQLite `mode=ro` 打开，**永不写入**。

> 这是 **v2 刚发布约一个月（2026-10 前后）的过渡期状态**，不是长期设计。
> opencode 的官方 V1→V2 迁移（`packages/core/src/database/v1-migration.bun.ts`）
> 在启动时自动跑，跑完把 KV `migration.v1-v2` 标成 `completed`；但迁移完成后
> 若又用旧版 opencode 写过会话，新版不会重扫 v1，那些会话就只留在 v1 表里
> （本机 2026-11-10 实测：全库 23 个 v1_only，重置 KV 状态并重启服务后归零）。
> 等 v1 表彻底不再产生新数据，这里的合并读取就可以删掉，只留 v2。

codebuddy 有**自动清除**机制（`cleanupPeriodDays`，默认 30 天，本机已设为 90），
过期会话连文件一起消失，aa-session 只能看见还没被删的那部分。详见
[`docs/codebuddy-retention.md`](docs/codebuddy-retention.md)。要长期归档必须导出，
别指望源目录自己留着。

codebuddy-ide 的 `history/<md5>` 目录名是**工作区路径的 md5（大小写敏感、反斜杠）**，
反查优先用 `genie-history` 的 base64 目录名，其次工作区引用目录名，最后从会话里的
`<user_info> Workspace Folder:` 兜底。消息文件路径常常超过 `MAX_PATH`，所有 I/O 必须
经 `core::long_path()` 加 `\\?\` 前缀。

claude 的 `assistant` 事件**不是一条消息**：同一条 API 消息的每个内容块各占一行，
靠 `message.id` 相同来标识；`usage` 在这几行上完全重复，统计 token 必须按
`message.id` 去重（按行累加会翻 N 倍）。元数据统计已处理，Markdown 归并尚未实现。

zed 是唯一实现了**写入**（`SessionImporter` 覆盖导入）的来源；另外四个
`open_importer()` 返回 `None`。

### 为什么直读而不是 `opencode serve` HTTP API

opencode 提供 `opencode serve`（`GET /session` 等）。不用它：

1. **它要求 opencode 进程在跑**，而核心场景恰恰是关着 opencode 时翻查历史；
2. 取全部消息正文要逐个 session 发 HTTP（891 个会话 / 20 万条消息），
   本地一次 SQL 即可；
3. 写路径本来就不该碰——那正是与运行中进程抢状态。

### 关于 `app`

`SessionSource::app()` 标识数据由哪个 agent 应用产生（当前实现：
`opencode` / `codebuddy` / `codebuddy-ide` / `claude` / `zed`）。它**同时是「来源」和「目标」**——将来 fork 时
「从 app A 的会话 fork 出 app A 的新会话」用的是同一个字段。不要叫 `source`，那会被误解成只读导入方向。

## 架构

```
core/                领域模型 + SessionSource trait（零外部依赖）
sources/opencode/    直读 opencode.db（rusqlite）
sources/codebuddy/   直读 ~/.codebuddy/projects/**/*.jsonl
sources/codebuddy-ide/ 直读 CodeBuddy IDE 的 history/**/index.json + messages/*.json
sources/claude/      直读 ~/.claude/projects/**/*.jsonl
sources/zed/         直读 threads.db + 覆盖导入（唯一写路径）
apps/cli/            clap 入口
```

依赖方向单向：`core` ← `sources/*` ← `apps/cli`。

**`core` 刻意不依赖 `rusqlite` / `anyhow`**——错误类型只带 `String` 描述，
库层用 `thiserror`，只有二进制入口用 `anyhow` 堆上下文。这样 GUI 阶段能直接
复用库而不必传递 `anyhow::Error`。

新增数据源只需再写一个 `SessionSource` 实现直读各自存储。JSONL 形态
（codebuddy 这类）已经跑通过一次，照抄 `sources/codebuddy` 最快。

## 导出格式

JSON 是**原始记录的无损镜像**（给编辑器和 `import`），Markdown 是**版式化对话**
（给人读）。dump 层不得归一、v1/v2 双写时只读 v2（仅指单会话正文渲染，
列表层仍是 v1+v2 合并，见上节）、`_index.json` 每次重写等硬性
约束见 [`.agents/export-format.md`](.agents/export-format.md)。

## 写操作与安全边界

**绝不对运行中的 `opencode.db` 做无备份写入**。`rename-project` / `import` 都默认
只预检，`--apply` 才写，且写前强制备份。完整规则、「零依赖零复用 zed」的依据、
ACP `session/fork` 的空白区见 [`.agents/safety.md`](.agents/safety.md)。

## 路线图

见 [`ROADMAP.md`](ROADMAP.md)（任务列表树维护）。下一步是 v2 的写操作
（`fork --at <seq>` / `edit`），前置约束见上节。

## 开发

```bash
cargo test --workspace                     # 33 个测试
cargo clippy --workspace --all-targets     # 0 warning
cargo build --release
just ci                                    # fmt + clippy + test，提交前跑这个
```

## 提交与推送

agent 自行决定，准则如下：

- **提交**：每完成一个可独立理解的逻辑单元，且 `just ci` 全绿，就提交一条 commit；
  不积累大块未提交的工作。
  - 消息用 conventional 风格（按改动实质选 `feat:` / `fix:` / `refactor:` /
    `docs:` / `chore:`），描述可写中文。
  - 提交前必须 `just ci` 全绿（fmt + clippy 0 warning + test 全过），不绿不提交。
- **推送**：当工作已完整交付（各逻辑单元都已提交、ci 全绿）时，可自行 push 到
  `origin` 当前分支；半成品或重构中途的状态不 push。
  - 永远不 `--force`、不改写已推送的历史。

## 环境与可用工具

- **remote**：`origin` = `git@github.com:Nahida-aa/aa_session.git`。
- **`gh` CLI 已安装且已登录**（账号 `Nahida-aa`，git 协议 ssh）。
  GitHub 相关操作（看/建 issue、PR、release、查远端状态等）直接跑 `gh`，
  不需要网页或其他途径。
