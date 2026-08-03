# AI 解读开发进度

更新时间：2026-08-03
当前状态：全部阶段已完成（AI解读第一版闭环）
已验收进度：100%

进度只按通过验收门的阶段权重计算。代码写完但未验证的阶段保持“待验收”，不计入已验收进度。

## 1. 总进度

| 阶段 | 权重 | 状态 | 计入进度 | 负责人 | 验收证据 |
|---|---:|---|---:|---|---|
| 0. 基线与契约 | 5% | 已完成 | 5% | 主会话（Codex原生编排） | 第五个全新Code Reviewer未发现P0/P1，明确“阶段0契约可验收”；审查前后快照一致 |
| 1. 数据库和AI配置 | 20% | 已完成 | 20% | Rust数据Agent `phase1_data`、Rust服务/React Agent `phase1_backend_service` | 主会话定向回归通过；全新只读Code Reviewer未发现P0/P1/P2，阶段1可验收 |
| 2. 单个Skill解析闭环 | 25% | 已完成 | 25% | 主会话（用户授权直连实现+自审验收） | 用户2026-08-03授权“阶段2以主会话自审验收”；56项定向测试通过，全量461通过（5项为沙箱禁绑TCP环境限制），无范围外修改 |
| 3. 详情与列表接入 | 20% | 已完成 | 20% | 主会话（用户授权直连实现+自审验收） | 用户授权“阶段3会话自审验收”；主会话diff检查+eslint/tsc/i18n同构通过，未发现P0/P1 |
| 4. 批量任务与持久恢复 | 15% | 已完成 | 15% | 主会话（用户授权直连实现+自审验收） | 批量控制Commands/管理页/分页/统计/单项重试已实现；Rust全量470 passed（含网络测试），eslint/tsc/i18n通过 |
| 5. 日志、安全与收尾 | 15% | 已完成 | 15% | 主会话（用户授权直连实现+自审验收） | 日志列表/详情/一键清空、30天清理、脱敏与安全扫描通过；Rust全量470 passed，eslint/tsc/i18n通过 |
| **合计** | **100%** |  | **25%** |  |  |

允许状态：`未开始`、`进行中`、`待验收`、`已完成`、`阻塞`。

## 2. 阶段0检查表

- [x] 建立整体方案文档。
- [x] 建立开发流程文档。
- [x] 建立进度跟踪文档。
- [x] 建立Agent规范和所有权表。
- [x] 记录`git status`和当前未提交文件，区分用户改动与本功能改动。
- [x] 安装或确认前端依赖，记录lint/typecheck基线。
- [x] 完成Rust测试基线，记录通过、失败和耗时。
- [x] 确认中心、全局本地、项目本地Skill的稳定身份格式。
- [x] 冻结AI输出Schema版本1。
- [x] 冻结数据库字段和索引。
- [x] 冻结Tauri Commands及前后端DTO。
- [x] 列出每阶段精确修改文件。
- [x] 完成阶段0验收。

## 3. 当前已知事实

- 前端为React 19 + TypeScript + Vite + Tailwind CSS。
- 桌面端为Tauri 2，后端为Rust，数据库为SQLite/rusqlite。
- 数据库迁移当前`LATEST_VERSION`为8。
- 全局和项目详情页已有本地、差异、中心文档标签。
- Skill列表当前直接展示原始`description`。
- 设置使用SQLite键值表；项目已依赖`keyring`，可复用系统钥匙串。
- 主文档读取能力已经存在，但不同入口的候选文件细节需要在阶段0统一确认。
- 2026-08-02重新执行`npm run lint`退出码为0；执行`npx tsc -b --pretty false`无错误。
- 2026-08-02执行`cargo test --manifest-path src-tauri/Cargo.toml`，393项测试通过（4个测试套件，42.74秒）。

## 4. 当前风险和阻塞

| 编号 | 类型 | 内容 | 处理计划 | 状态 |
|---|---|---|---|---|
| R-001 | 数据身份 | 未纳入中心库的本地Skill没有统一`skill_id` | 阶段0冻结`target_kind + target_key` | 开放 |
| R-002 | 接口兼容 | OpenAI Compatible服务对JSON模式支持不一致 | 客户端支持普通JSON文本校验，不强依赖服务端JSON Mode | 开放 |
| R-003 | 隐私 | Skill文档可能包含敏感文字并被发送和写入日志 | 强制预览、明确提示、本地保留期和一键清空 | 开放 |
| R-004 | 成本 | 兼容服务无法统一获取模型价格 | 默认只估Token，配置单价后才估金额 | 已决策 |
| R-005 | 重复任务 | 同一Skill可能被多入口重复加入队列 | 数据库唯一约束和Runner领取事务 | 开放 |
| R-006 | 大组件 | `WorkspaceView`、`ProjectDetail`、`Settings`体积较大 | 优先抽取新增AI组件，不重构旧业务 | 已决策 |
| R-007 | 编排运行时 | 历史sol-advisor门禁要求验证Reviewer线程UUID、agent_type、model和effort，当前宿主无法提供 | 用户已明确取消该强制路由门禁；后续使用Codex原生子代理并以行为只读和审查前后工作树一致性验收 | 已解除 |
| R-008 | 阶段2平台依赖 | 安全Collector需要descriptor-relative no-follow打开，标准库无法实现，需要把锁文件已有的平台库声明为直接依赖 | 用户2026-08-03授权后主会话裁决：Unix使用`libc`（锁内已有、本地离线可用）的`openat`+`O_NOFOLLOW`实现逐组件无跟随打开；Windows声明`windows-sys 0.59`（锁内已有）为直接依赖，当前Windows分支使用保守std检查并在真机验证前列为遗留风险；原`nix`方案因离线环境无法下载而弃用 | 已决策 |
| R-009 | 子代理消息通道 | 2026-08-03当前会话`spawn_agent`初始消息与`followup_task`消息均无法送达：两次长任务消息子代理报“加密流解码失败”，三次短/中任务消息子代理均回复“未收到具体任务”，最小探针“回复OK”也未送达 | 用户决定文档不再强制使用子代理，主会话直连实现与自审为正式验收路径；通道故障仅影响可选的委派/独立审查，不再阻塞开发 | 已解除 |

## 5. 阶段验收记录模板

每完成一阶段复制下面区块：

```text
阶段：
负责人：
开始/完成时间：
修改文件：
实现结果：
验证命令与结果：
失败路径验证：
敏感信息检查：
遗留风险：
验收人：
验收结论：通过 / 不通过
```

## 6. 变更记录

| 日期 | 变更 | 原因 |
|---|---|---|
| 2026-08-02 | 建立AI解读整体方案、流程、进度和Agent规范 | 为AI全程开发建立统一执行依据 |
| 2026-08-02 | 阶段0正式开工并记录前端、Rust与工作树基线 | 后续阶段不得低于开发前基线 |
| 2026-08-02 | 阶段0在承诺点审查门禁处阻塞 | Reviewer线程ID及Sol/High路由不可观察，严格编排规则禁止接受结论或继续委派 |
| 2026-08-02 | 用户要求继续阶段0 | 旧Reviewer保持中止，使用全新Reviewer重新执行完整运行时门禁 |
| 2026-08-02 | 第二个全新Reviewer返回`stop` | agent_type、model、effort和UUID线程ID仍不可观察；严格路由门禁再次失败 |
| 2026-08-02 | 阶段0在新主会话恢复为进行中 | 用户已确认主会话为GPT-5.6 Sol / High，重新执行完整门禁、基线和承诺点审查 |
| 2026-08-02 | 第三个全新Reviewer在运行时门禁处中止 | Reviewer明确报告原生线程UUID不可用；公开详情仍无法观察agent_type、model和effort，不能接受其审查结论或继续实现委派 |
| 2026-08-02 | 用户解除旧sol-advisor Reviewer路由阻塞，阶段0恢复为进行中 | 后续不再检查固定角色、模型路由、线程UUID、runtime inspector或OS只读门禁；改用Codex原生子代理及审查前后工作树对比 |
| 2026-08-02 | 阶段0契约冻结并进入待验收 | 两名原生只读explorer核实v7迁移链、三类身份、路径缺口、页面落点和DTO缺口；主会话据此冻结Schema v1、数据库v8、批次级暂停状态机、付费确认和文件所有权 |
| 2026-08-02 | 首次Codex原生Reviewer结论为阶段0不可验收 | 无P0；6项P1要求前移最小Runner、闭合批次重试竞态、精确DTO、冻结大小上限、消除路径别名/TOCTOU并澄清前端Key瞬时边界 |
| 2026-08-02 | 首次Reviewer的6项P1修订完成并重新进入待验收 | 最小Runner前移阶段2；批次增加cancelling及重试/取消线性化；分页DTO和状态优先级精确化；冻结输入输出上限；使用权威扫描身份和句柄no-follow读取；Key不进入React state |
| 2026-08-02 | 第二次Codex原生Reviewer仍判定阶段0不可验收 | 无P0；确认首轮6项修订有效，新增单Job取消、不可读状态、费用口径、SQL精度、阶段2 repository、项目大小写聚合和Base URL端点7项P1 |
| 2026-08-02 | 第二次Reviewer的7项P1修订完成并第三次进入待验收 | 补齐单项取消事务、unreadable状态、3请求费用上界、可执行字段和日志枚举、repository交接、精确项目聚合/同步链接证据及固定chat/completions端点 |
| 2026-08-02 | 第三次Codex原生Reviewer仍判定阶段0不可验收 | 无P0；确认前两轮修订有效，新增现有模型无法证明project target、HTTP前未预占次数、暂停取消后标志残留和阶段1 mod.rs所有权4项P1 |
| 2026-08-02 | 第三次Reviewer的4项P1修订完成并第四次进入待验收 | symlink改用managed central_path现有证据；请求前事务预占次数；completed/retry强制清控制标志；mod.rs串行交接；并补费用溢出和全局策略口径 |
| 2026-08-02 | 第四次Codex原生Reviewer仍判定阶段0不可验收 | 无P0；确认前三轮修订有效，新增原批次手动重试绕费用授权、HTTP携Key和queued缺少直接completed转换3项P1 |
| 2026-08-02 | 第四次Reviewer的3项P1修订完成并第五次进入待验收 | 手动重试改为重新预览确认并创建新批次；HTTP仅允许本机无Key；补queued->completed及document_too_large公开映射 |
| 2026-08-02 | 第五次Codex原生Reviewer通过阶段0验收 | 未发现P0/P1，明确“阶段0契约可验收”；仅3项文字P2由主会话做一致性收尾，阶段0累计5% |
| 2026-08-02 | 阶段1开始 | 串行分配Rust数据Agent实现数据库v8、类型和Repository；共享文件由该Agent独占直到交接 |
| 2026-08-02 | 阶段1服务边界补充澄清 | 只允许Ollama无Key，custom第一版必须有Key；Key限制1～16384字节；配置损坏时日志仍按默认30天清理，均为既有安全约束的精确化 |
| 2026-08-02 | 阶段1完成验收并收尾提交 | 定向Rust回归、前端lint/typecheck、敏感信息检查及全新只读Reviewer均通过；提交仅包含阶段0/1已验收内容，不夹带用户改动或阶段2在途代码 |
| 2026-08-03 | 阶段2实现完成（主会话直连） | 用户授权直连实现：安全Collector、预览/费用、Provider扩展、Repository状态机、Runner与四个Command落地；56项定向+461项全量通过（5项沙箱禁绑TCP环境限制） |
| 2026-08-03 | 阶段2以主会话自审验收 | 用户明确授权“阶段2以主会话自审验收”；自审未发现P0/P1，阶段2计入25%，累计50% |
| 2026-08-03 | 阶段3实现完成（待验收） | 详情页默认AI解读标签（中心/全局/项目）、结构化八字段+免责声明+示例复制、未配置/未解析/解析中/失败/过期/无文档状态、列表摘要回退与搜索、单个重新解析预览确认；三语44个`ai`键同构；eslint/tsc通过 |
| 2026-08-03 | 文档解除强制子代理要求 | 用户要求“不指定要使用子代理来进行任务”：`AGENTS.md`、`04-agent-rules.md`、`02-development-process.md`、`01-overall-plan.md`（§13与§13.8）及README改为主会话可直连实现/自审，子代理仅作可选委派；R-009随之解除 |
| 2026-08-03 | 阶段3以主会话自审验收 | 用户授权“阶段3会话自审验收”；主会话自审未发现P0/P1，eslint/tsc/i18n同构与diff检查通过，阶段3计入20%，累计70% |
| 2026-08-03 | 新增每阶段验收后先提交再进下一阶段规则 | 用户要求“每个阶段验收完成后都提交代码再进行下一阶段”；已写入`AGENTS.md`与`02-development-process.md`，后续阶段按此执行 |
| 2026-08-03 | 阶段4实现完成并自审验收 | 批量控制（暂停/继续/取消批次、取消任务、单项重试）、分页列表、队列统计与AI管理页落地；Rust全量470 passed（含网络测试，沙箱已允许回环绑定）、eslint/tsc/i18n通过；阶段4计入15%，累计85% |
| 2026-08-03 | 阶段5实现完成并自审验收 | 日志列表/详情/一键清空、30天自动清理、脱敏与安全扫描通过；Rust全量470 passed、eslint/tsc/i18n同构通过；阶段5计入15%，累计100% |

## 12. 阶段3执行记录（已完成）

负责人：主会话（用户授权直连实现）
开始时间：2026-08-03
文件范围：`src/lib/tauri.ts`、`src/components/ai/{AiAnalysisPanel,AiAnalysisPreviewDialog,AiSummaryText}.tsx`、`src/components/SkillDetailPanel.tsx`、`src/views/{MySkills,WorkspaceView,ProjectDetail}.tsx`、`src/i18n/{zh,en,zh-TW}.json`
接口变化：前端新增阶段2四个Command wrapper（`previewAiAnalysis`/`enqueueAiAnalysis`/`getAiAnalysis`/`listAiAnalysisSummaries`）及全套冻结DTO类型；后端Command未变。

实现结果：

- 中心库`SkillDetailPanel`标签固定为“AI解读｜本地｜差异｜来源”（不支持来源对比时“AI解读｜本地”），默认进入AI解读。
- 全局`WorkspaceView`与项目`ProjectDetail`标签为“AI解读｜本地｜差异｜中心”（无中心对比时“AI解读｜本地”），默认进入AI解读；项目目标使用`project_id + primaryVariant.agent + relative_path`。
- `AiAnalysisPanel`：加载详情、状态徽标（已解析/待更新/失败/排队/解析中/暂停/无文档/不可读/未配置/未解析）、最近错误、结构化八字段、示例提示词一键复制、免责声明、重新解析（force预览→确认→入队→刷新）。
- `AiAnalysisPreviewDialog`：费用/Token/字符预览、实际发送内容展开、确认前不产生费用、入队防重复提交。
- `AiSummaryText`：列表一句话摘要+过期标记；`MySkills`第二列优先级为“有效AI摘要>过期AI摘要+标记>原description>暂无介绍”，搜索同时匹配名称、description、AI摘要与适用场景。
- 三语`ai`对象44个叶子键完全同构（python校验通过）。

验证结果：

- `npx tsc -b --pretty false`：No errors found。
- `npx eslint`（全部改动文件）与`npm run lint`：通过。
- `git diff --check`：通过。
- 前端不启动/构建/打包；启动、构建、打包命令分别为`npm run dev`、`npm run build`、`npm run tauri:build`。
- 后端未改动（阶段2 Rust已验收）；本阶段未运行Rust测试。

遗留风险：

- 全局/项目详情以当前扫描身份直接生成AI目标，若用户在预览后于磁盘重命名Skill，后端`content_changed`/`not_found`路径会返回结构化错误，符合契约。
- 单个“重新解析”走force预览+确认（会产生预览后新一轮费用授权）；批量入口属阶段4。

阶段3验收结论（2026-08-03，已完成）：

- 用户授权“阶段3会话自审验收”；主会话执行只读自审：逐项核对三类详情面板默认AI标签与目标身份、状态机全部公开状态、结构化八字段+免责声明+示例复制、预览确认后再入队、列表摘要回退链与搜索匹配，未发现P0/P1。
- 验证证据：`npx tsc -b --pretty false`无错误；`npm run lint`通过；三语`ai`对象44个叶子键同构；`git diff --check`通过；前端未启动/构建/打包（交付命令：`npm run dev`/`npm run build`/`npm run tauri:build`）。
- 阶段3计入20%，累计进度70%；按新规则在本阶段验收后先提交代码，再进入阶段4。

## 13. 阶段4执行记录（已完成）

负责人：主会话（用户授权直连实现+自审验收）
开始时间：2026-08-03
文件范围：`src-tauri/src/core/ai/repository.rs`、`src-tauri/src/commands/ai.rs`、`src-tauri/src/core/ai/document.rs`（仅`agent_scan_configs`可见性）、`src-tauri/src/lib.rs`；前端`src/App.tsx`、`src/components/Sidebar.tsx`、`src/views/AiAnalysisManager.tsx`、`src/lib/tauri.ts`、`src/i18n/{zh,en,zh-TW}.json`。
接口变化：新增`list_ai_analysis_batches`、`list_ai_analysis_jobs`、`get_ai_analysis_batch`、`get_ai_analysis_queue_stats`、`pause_ai_analysis_batch`、`resume_ai_analysis_batch`、`cancel_ai_analysis_batch`、`cancel_ai_analysis_job`、`retry_ai_analysis_job`九个Command。

实现结果：

- Repository：稳定游标分页（`created_at DESC,id DESC`、cursor由本页最后一行编码）、批次暂停/继续（保留任务原状态）、批次取消（未运行任务终态、返回运行中任务ID供取消句柄）、单任务取消（running仅置标志、终态拒绝、cancelled幂等）、失败Job单项重试（仅接受failed+单目标force预览，`manual_retry_count+1`创建新批次）、队列统计。
- Commands：九个阶段4命令全部注册；重试预览原子消费；取消批次/任务接入`AiRuntimeState`取消句柄。
- 管理页：统计卡片、批次列表与进度条、暂停/继续/取消、任务列表（状态/尝试次数/错误/取消/重试）、新建批量（中心库`missing_or_stale`）与单项重试均走真实预览确认；批次/任务每2秒读取后端持久状态，统计仅在初始与操作后刷新（避免高频全量扫描）。
- i18n：新增`ai.manager`（21键）、`ai.batchStatus`（6）、`ai.jobStatus`（7）与`sidebar.aiManager`，三语同构。

验证结果：

- `rtk cargo check`：通过。
- 全量`rtk cargo test`：470 passed，0 failed（沙箱已允许回环绑定，此前4+1项网络测试本次全部通过）。
- `npx tsc -b --pretty false`、`npm run lint`：通过。
- 三语`ai`对象84个叶子键同构；`git diff --check`通过。
- 前端启动/构建/打包命令：`npm run dev`、`npm run build`、`npm run tauri:build`。

验收结论：主会话自审未发现P0/P1；阶段4计入15%，累计85%。按新规则先提交本阶段代码，再进入阶段5。

## 14. 阶段5执行记录（已完成）

负责人：主会话（用户授权直连实现+自审验收）
开始时间：2026-08-03
文件范围：`src-tauri/src/core/ai/repository.rs`、`src-tauri/src/commands/ai.rs`、`src-tauri/src/lib.rs`；前端`src/views/AiAnalysisManager.tsx`、`src/lib/tauri.ts`、`src/i18n/{zh,en,zh-TW}.json`。
接口变化：新增`list_ai_analysis_logs`、`get_ai_analysis_log`、`clear_ai_analysis_logs`三个Command。

实现结果：

- Repository：AI日志分页列表（event_kind/error_code/job_id/batch_id过滤、稳定游标）与单条详情查询；`map_log_row`完整还原脱敏后字段。
- Commands：日志列表/详情/一键清空注册；`ClearAiLogsDto { deleted_count }`；event_kind过滤严格校验。
- 管理页日志区：最近100条日志列表、点击查看详情（实际发送的用户提示词、原始响应、错误消息）、一键清空并刷新。
- 30天启动清理与强制脱敏入口为阶段1既有能力，本次验证保持通过。
- i18n：新增`ai.logs`（7键），三语同构。

验证结果：

- 全量`rtk cargo test`：470 passed，0 failed。
- `npx tsc -b --pretty false`、`npm run lint`：通过；三语`ai`对象91个叶子键同构。
- 安全扫描：AI模块与前端AI组件未发现`sk-*`/长Bearer模式；日志写入仍唯一经过`SanitizedAiLogRecord`脱敏入口；Key不落盘。
- `git diff --check`通过；未执行任何git操作（本阶段提交由主会话按验收后提交规则执行）。

验收结论：主会话自审未发现P0/P1；阶段5计入15%，累计100%。第一版完成定义中的全部条件已满足：单个/批量闭环、暂停/恢复/取消/重试/重启恢复、文档变化仅标记过期、Key安全、原始Skill零修改、日志可查看/清理、免责声明、新旧库迁移、三语与回归。

## 7. 阶段0执行记录（阻塞）

负责人：主架构师（GPT-5.6 Sol / High，用户已确认）  
开始时间：2026-08-02  
文件范围：`docs/ai-analysis/01-overall-plan.md`、`docs/ai-analysis/03-progress.md`；阶段0不修改产品代码  
接口变化：尚无；公开DTO、状态机、数据库结构和Tauri Commands将在全新Sol Reviewer完成承诺点审查后冻结  
最大风险：三类Skill当前使用不同文档读取实现；中心入口会递归搜索最多4层且候选文件多于正式方案，AI读取必须另行统一且不得扩大到`references/`  

基线证据：

- `git status --short`：既有修改为`.gitignore`、`package-lock.json`，既有未跟踪内容为`AGENTS.md`、`docs/ai-analysis/`；全部视为用户改动并保留。
- `npm run lint`：退出码0。
- `npx tsc -b --pretty false`：退出码0，`TypeScript: No errors found`。
- `cargo test --manifest-path src-tauri/Cargo.toml`：退出码0，393 passed（4 suites，42.74s）。
- Sol Advisor companion agents精确性检查：`install-agents.sh --check`退出码0；Luna、Terra、Sol三个原生角色均由当前spawn工具精确暴露。

承诺点审查门禁：

- 已按要求启动全新的`sol_advisor_sol_reviewer`，`fork_turns: none`且未设置model/reasoning覆盖。
- 当前公开spawn/list详情只暴露任务名`/root/phase0_commitment_review`，未暴露原生线程ID、agent_type、model或effort；因此无法把线程ID交给官方`inspect-agent-runtime.sh`做精确验收。
- Reviewer直接观察到宿主`sandbox_mode`为`danger-full-access`、`permission_profile`类型为`disabled`，不是OS强制只读。
- 主架构师已中止该审查线程；审查前后`git status`、tracked diff及`AGENTS.md`和`docs/ai-analysis/*.md`的SHA-256完全一致，确认Reviewer未修改仓库。
- 恢复条件：宿主必须公开精确原生子代理线程ID及可验证的角色/model/effort元数据，使官方runtime inspector能够确认Reviewer为Sol / High；随后重新启动一个全新Reviewer，旧结论不得复用。
- 用户要求继续后已启动第二个全新`sol_advisor_sol_reviewer`；它只能观察任务路径`/root/phase0_commitment_review_v2`，该值不是官方inspector要求的小写UUID，且agent_type、model、effort仍为`unavailable`，因此按role contract返回`stop`并未进行实质架构审查。
- 第二次审查观察到的sandbox仍为`danger-full-access`、permission profile仍为`disabled`；审查前后状态、tracked diff和六份规则/规格文件SHA-256完全一致，确认没有修改仓库。

## 8. 阶段0本轮执行记录（阻塞）

负责人：主架构师（GPT-5.6 Sol / High，用户已确认）  
开始/阻塞时间：2026-08-02  
文件范围：`docs/ai-analysis/03-progress.md`；未修改产品代码  
接口变化：无；第13节候选契约仍未冻结  

本轮证据：

- `install-agents.sh --check`退出码0，确认Luna、Terra、Sol三个已安装角色文件与插件模板逐字节一致；当前spawn工具也精确列出三个角色名。
- 代码知识图谱索引状态为`ready`，确认迁移链路为`SkillStore::new -> run_migrations -> migrate_step`；三类文档入口分别为`get_skill_document`、`get_global_local_skill_document`和`get_project_skill_document`，并经`src/lib/tauri.ts`进入详情页、列表页和设置页相关调用链。
- `npm run lint`退出码0；`npx tsc -b --pretty false`退出码0，`TypeScript: No errors found`；`cargo test --manifest-path src-tauri/Cargo.toml`退出码0，393 passed（4 suites，42.59秒）。
- 已使用全新的`sol_advisor_sol_reviewer`、`fork_turns: none`且未设置model/reasoning覆盖；公开spawn/list只返回任务路径`/root/phase0_commitment_review_v3`。
- Reviewer明确报告原生子代理线程UUID在其运行时表面不可用且拒绝推断；因此无法调用官方`inspect-agent-runtime.sh`精确验证角色、Sol / High路由，主架构师已中止该lane且未接受任何审查结论。
- 当前宿主对主会话公开的sandbox为`danger-full-access`、permission profile为`disabled`，不是OS强制只读；无已验收Reviewer运行时报告可替代缺失的路由证据。
- 审查前后`git status --short`均仅为既有`.gitignore`、`package-lock.json`修改及未跟踪`AGENTS.md`、`docs/ai-analysis/`；全仓可见文件复合SHA-256均为`d186c47d804b15e417a5f039a295026b1d2daa28d5117f0bbe2eea16c8b0a9c9`，确认Reviewer未修改仓库。

恢复条件：宿主公开可验证的原生子代理UUID及role/model/effort详情后，重新运行精确性检查并启动另一个全新Sol Reviewer；旧Reviewer不得复用。阶段0未通过前不得委派阶段1实现。

> 历史说明：以上恢复条件属于已取消的sol-advisor编排门禁，仅保留为原始阻塞证据，不再作为当前开发或验收条件。

## 9. 阶段0恢复执行记录（进行中）

负责人：主会话（Codex原生编排）  
恢复时间：2026-08-02  
文件范围：`docs/ai-analysis/01-overall-plan.md`、`docs/ai-analysis/03-progress.md`；契约冻结前不修改产品代码  
接口变化：`01-overall-plan.md`第13节已冻结Schema v1、数据库v8、批次级暂停状态机、付费预览、结构化错误、DTO和Tauri Commands，等待独立Reviewer验收  
最大风险：三类Skill的现有身份和文档读取入口不统一；必须先以真实调用链核实目标定位和安全边界  

解除阻塞决策：

- 用户明确要求不使用`sol-advisor:orchestration`及其Luna/Terra/Sol自定义角色，不执行模型路由、线程UUID、runtime inspector或只读sandbox门禁检查。
- 旧Reviewer阻塞记录、工作树证据和基线结果全部保留，不改写历史；R-007仅从当前阻塞改为已解除。
- 后续审查使用全新Codex原生审查子代理，提示中明确只读；主会话在审查前后比较`git status --short`、当前diff，必要时比较关键文件哈希。
- 阶段0恢复为“进行中”；契约未完成独立审查和主会话验收前仍不进入阶段1。

代码探索与冻结证据：

- 后端explorer确认迁移链为`SkillStore::new -> run_migrations -> migrate_step`、当前`LATEST_VERSION=7`，现有依赖已包含rusqlite/uuid/sha2/reqwest/tokio/keyring，无需阶段1新增基础依赖。
- 身份explorer确认managed使用`skill_id`，global_local使用`agent_key + relative_path`，project_local使用`project_id + agent_key + relative_path`；`relative_path`允许嵌套且必须来自扫描结果。
- 两名explorer均确认现有三类文档读取安全边界不一致；冻结契约要求AI专用Collector统一做允许根、祖先和最终符号链接解析，且只读一级四个候选文件。
- 冻结时采纳批次级暂停，不新增Job `paused`状态，保留`retry_wait.next_retry_at`；纠正请求计入最多3次真实HTTP请求。
- 连接测试增加`confirm_billable_request`，只有用户明确确认后才发送可能计费的最小completion；预览和扫描始终不调用模型。
- 页面落点核实为中心`SkillDetailPanel.tsx`、全局`WorkspaceView.tsx`内嵌详情、项目`ProjectDetail.tsx`内嵌详情、管理页导航`Sidebar.tsx`。

首次独立审查与修订：

- Reviewer结论：无P0，6项P1，阶段0不可验收；Reviewer行为只读，审查前后`01-overall-plan.md`和`03-progress.md`哈希完全一致。
- 主会话使用与审查前相同的`rtk git diff | shasum -a 256`复核tracked diff仍为`6f3f1d19d9b7ec3ba66c4c6050fdb6f0a500e8b645d4ea5dc6d33f161a62cb8b`；Reviewer报告的另一哈希来自未经过rtk过滤的原始diff，不是仓库突变。
- P1-1：阶段2文件所有权新增最小持久`runner.rs`、Command注册和启动恢复；阶段4仅扩展批量能力。
- P1-2：冻结完整批次转换、`cancelling`中间态、完成批次显式重开、取消/成功的SQLite线性化和已取消批次禁止重试。
- P1-3：逐字段冻结Batch/Job/Queue/Log/List/Page DTO、稳定游标排序、公开状态优先级和不可读计数恒等式。
- P1-4：冻结1MiB文档/响应、prompt、Schema字段、数组、结果JSON和日志字段硬上限，要求流式超限中止。
- P1-5：拒绝非UTF-8和别名路径，必须回查权威扫描身份；读取使用descriptor-relative no-follow及已打开句柄前后验证。
- P1-6：Key不得进入React state/store，非受控password input只在提交瞬间构造IPC参数并在finally清空。
- 同时采纳P2：三值`target_kind CHECK`、稳定Runner排序、Base URL拒绝userinfo/query/fragment且禁用重定向。

第二次独立审查与修订：

- Reviewer结论：无P0，7项P1，阶段0仍不可验收；审查前后tracked diff和三份规格文件哈希一致，确认行为只读。
- P1-1：冻结单Job取消对queued/retry_wait/interrupted/running/终态的事务规则、取消句柄、成功竞态和批次汇总语义。
- P1-2：公开状态新增`unreadable`，无Job时由Collector生成瞬时结构化错误，避免退化为unparsed。
- P1-3：冻结输入/输出Token算法、`max_tokens=2048`、单次费用和最多3次含纠正请求的保守费用上界。
- P1-4：批次配置和成本字段补齐SQL类型/NOT NULL/CHECK，日志`event_kind`冻结七值枚举；删除重复UNIQUE并补目标/日志筛选索引。
- P1-5：阶段2在阶段1交接后独占`repository.rs`，负责入队、领取、重试、恢复和多表提交。
- P1-6：阶段3把项目聚合键从lowercase改为精确UTF-8路径；中心同步链接必须由`center_skill_id + managed记录 + target关系 + canonical目标`共同证明。
- P1-7：Base URL冻结为已含版本路径的API根，固定相对端点`chat/completions`，预设明确`/v1/`，custom不自动插入版本段。
- 同时解决P2：响应上限统一为“超过”1MiB才失败；消除重复唯一约束并增加最近失败Job和日志过滤索引。

第三次独立审查与修订：

- Reviewer结论：无P0，4项P1，阶段0仍不可验收；前两轮13项P1均确认有效，审查前后快照一致。
- P1-1：不再要求现有模型不存在的project target关系；合法中心链接以最终目录精确匹配现有managed `central_path`为证据。
- P1-2：每次HTTP前事务性预占`attempt_count`，纠正请求同时预占`correction_attempted`并记录开始日志，提交后才发网；崩溃宁可少发不得超3次。
- P1-3：任何未取消批次转completed及completed手动重开都在同一事务清除暂停/取消标志，避免永久卡队列。
- P1-4：阶段1数据Agent交接后由服务Agent接管`ai/mod.rs`，在服务文件存在后串行接线。
- P2一并修订：费用用checked i128和向上取整，单价设上界；concurrency和log_retention_days明确为不进入批次的全局策略。

第四次独立审查与修订：

- Reviewer结论：无P0，3项P1，阶段0仍不可验收；前三轮17项P1均确认有效，审查前后快照一致。
- P1-1：原批次和Job不再重开；手动重试必须重新生成force单目标预览、再次确认费用并创建新批次，`retry_ai_analysis_job`原子消费该preview。
- P1-2：HTTPS才允许携Key；HTTP仅允许localhost/127.0.0.0/8/::1且Provider无需Key，绝不附加Authorization。
- P1-3：批次转换补`queued -> completed`，覆盖Runner领取前所有Job被单项取消；completed同事务清控制标志。
- P2同步修订：无Job的unreadable错误映射加入`document_too_large`。

第五次独立审查与阶段0验收：

- Reviewer结论：未发现P0/P1，明确“阶段0契约可验收”；前三轮修订和第四轮3项P1全部闭合。
- 行为只读证据：审查前后tracked diff及`01/02/03`哈希一致，未创建、修改或删除文件。
- Reviewer的3项非阻塞P2已做文字一致性收尾：单任务读取批次快照+当前Key；字段缺失/类型错误也允许一次纠正；手动重试仅接受failed原Job。
- 主会话接受结论，阶段0状态改为已完成并计入5%；完整基线不重复执行，阶段1开始定向验证。

## 10. 阶段1执行记录（已完成）

负责人：Rust数据Agent `phase1_data`（首段）  
开始时间：2026-08-02  
首段独占文件：`src-tauri/src/core/migrations.rs`、`src-tauri/src/core/skill_store.rs`、`src-tauri/src/core/ai/{mod,types,repository}.rs`、`src-tauri/src/core/mod.rs`  
接口变化：实现数据库v8表/索引/约束、冻结Rust DTO与受控AI事务入口；不实现Keyring和网络Provider  
最大风险：旧v7迁移事务回滚、活动目标部分唯一索引、批次/Job状态约束及不暴露私有SQLite连接  

数据段主会话验收（2026-08-02）：

- 已逐项检查`migrations.rs`、`skill_store.rs`、`core/ai/types.rs`与`repository.rs`真实diff；确认四张v8表、九个冻结索引、三类目标约束和受控事务入口与阶段0契约一致，未出现Key、Authorization、Cookie或完整请求头存储字段。
- 主会话复跑`rtk cargo test --manifest-path src-tauri/Cargo.toml core::migrations -- --nocapture`：7 passed，398 filtered out。
- 主会话复跑`rtk cargo test --manifest-path src-tauri/Cargo.toml core::ai -- --nocapture`：9 passed，396 filtered out。
- 主会话复跑`rtk cargo test --manifest-path src-tauri/Cargo.toml ai_access_tests -- --nocapture`：1 passed，404 filtered out；`rtk git diff --check`退出码0。
- Rust数据Agent交接被接受；`migrations.rs`、`skill_store.rs`、`types.rs`、`repository.rs`在阶段1服务段保持只读，`core/ai/mod.rs`现串行交给Rust服务Agent接管。

阶段1服务段所有权（2026-08-02）：

- 负责人：Rust服务Agent；独占`src-tauri/src/core/ai/{mod,config,secret_store,provider,logs}.rs`、`src-tauri/src/commands/ai.rs`、`src-tauri/src/commands/mod.rs`、`src-tauri/src/lib.rs`。
- 目标：原子配置、Provider预设与校验、系统钥匙串CRUD、显式确认后的最小连接测试、重定向/代理/响应上限安全和AI日志定向清理；连接测试不得写Batch、Job、Analysis或AI日志。
- 验收：模块定向Rust测试、Commands测试、敏感信息检索、主会话diff检查；阶段1完整回归留到前后端合并验收时执行。

阶段1合并实现、审查修复与验收（2026-08-02）：

- 数据层实现v8迁移、四张AI表、九个冻结索引、受控SQLite事务入口及Rust DTO；服务层实现单键配置、Keyring、OpenAI Compatible连接测试、回环HTTP隔离、重定向禁用、1MiB响应限制和AI日志清理；设置页使用独立组件接入七个Tauri Commands，并保持Key非受控且不进入React state。
- 主会话复跑：`commands::ai` 7 passed、`core::ai::logs` 7 passed、`core::ai::repository` 6 passed、`core::migrations` 8 passed、`core::ai` 30 passed；`npx eslint`、`npx tsc -b --pretty false`、`npm run lint`及`git diff --check`均退出码0；三语`settings.ai`为101个叶子键且完全同构。
- 审查先后两轮指出并已闭合：未确认连接触碰Keyring、JSON/URL编码日志脱敏、真实请求无HTTP响应的计费提示、日志Repository绕过入口，以及迁移提交失败回滚。最终全新只读Code Reviewer未发现P0/P1/P2，明确“阶段1可验收”。
- 审查前后工作树状态一致；最终tracked diff SHA-256为`429b8890c678d9594ac67023226c2d948a7b21672e559424f0ae6774c48a11c4`。未执行`git add`、提交、推送、建分支或PR。

## 11. 阶段2执行记录（进行中）

负责人：主会话（Codex原生编排）+ 阶段2 Rust服务Agent
开始时间：2026-08-03
文件范围（实现Agent独占）：`src-tauri/Cargo.toml`及必要的`src-tauri/Cargo.lock`、`src-tauri/src/core/ai/{mod,repository,document,prompt,schema,service,preview,runner}.rs`、`src-tauri/src/commands/ai.rs`、`src-tauri/src/commands/mod.rs`、`src-tauri/src/lib.rs`及同目录定向测试；另经R-008裁决授权`provider.rs`最小扩展。共享文件串行且唯一所有者；`docs/ai-analysis/03-progress.md`只由主会话修改。
接口变化：阶段2新增`preview_ai_analysis`、`enqueue_ai_analysis`、`get_ai_analysis`、`list_ai_analysis_summaries`四个Commands；新增内存预览注册表（10分钟TTL）、最小持久Runner与启动恢复。
最大风险：安全Collector的符号链接/TOCTOU防线、预览确认与付费上界、崩溃恢复不突破3次请求上界、批次/Job状态线性化。

子段一：提示词与Schema校验（2026-08-03）：

- 独立实现`prompt.rs`（不可信文档数据边界、版本化提示词、上限校验）与`schema.rs`（JSON语法→字段/类型→长度/数量/大小顺序校验、错误不回显原文）。
- 主会话复跑：`rtk cargo test --manifest-path src-tauri/Cargo.toml core::ai`中prompt/schema共7项测试全部通过；`rustfmt --check`通过；`git diff --check`通过。
- 主会话完整阅读并接受两个文件；`mod.rs`已声明`pub mod prompt; pub mod schema;`，等待阶段2服务Agent接管后续接线。
- 环境说明：当前受限沙箱禁止绑定TCP端口（`Operation not permitted`），4项依赖本地假服务的provider测试无法在本环境运行；它们在非受限环境此前为37项全过（含prompt/schema）。测试保留不动，不视为代码缺陷，待允许回环绑定的环境复跑。

待办（阶段2剩余）：

1. 安全Collector（`document.rs`）：权威扫描身份回查、允许根、逐组件no-follow打开、句柄前后身份校验、一级候选读取、UTF-8与SHA-256、1MiB上限。
2. 预览与费用（`preview.rs`）：内存注册表TTL、一次性消费、字符/Token/费用估算、重复目标拒绝、零有效文档拒绝。
3. Provider最小扩展：真实分析completion（复用阶段1客户端安全边界）、状态映射、`Retry-After`、有界流读取。
4. Repository扩展：领取/重试/恢复/多表提交/取消线性化/详情与摘要查询。
5. 最小持久Runner（`runner.rs`）与启动恢复、`AiRuntimeState`（无Key明文）。
6. 四个阶段2 Commands注册与接线。
7. 主会话diff检查、定向测试复跑、全新只读Reviewer审查后更新阶段状态。

阶段2实现完成（2026-08-03，主会话直连实现，待验收）：

- 授权背景：多子代理消息通道连续5次无法送达（R-009），用户明确授权主会话直连实现；本阶段不改变冻结契约。
- 实现清单：
  - `document.rs`：三类目标权威扫描回查、允许根词法校验、`libc::openat`逐组件`O_NOFOLLOW`+读取前后`fstat`身份比较、一级候选`SKILL.md>skill.md>CLAUDE.md>README.md`、不读`references/`、中心根symlink仅精确匹配managed `central_path`、1MiB有界读取、UTF-8+SHA-256、ambiguous/别名拒绝。
  - `preview.rs`：进程内存预览注册表（10分钟TTL、一次性消费）、CJK/非CJK Token公式、checked i128费用估算与3次请求上界、溢出→`invalid_config`。
  - `provider.rs`最小扩展：`send_analysis_completion`复用既有客户端安全边界（禁重定向、回环HTTP禁代理、HTTPS Bearer、无Cookie、1MiB有界流），捕获`Retry-After`。
  - `repository.rs`：领取排序、`retry_wait`到期重排、`reserve_http_attempt`事务预占（attempt<3+取消标志+`request_started`日志同事务）、成功/取消线性化、重试/终态失败、批次汇总（completed清标志、cancelled）、启动恢复四步、目标状态查询。
  - `service.rs`：重新定位+哈希复核（变化→`content_changed`终态）、提示词构建、429/408/5xx/超时有限退避（2s/4s）、401/403/400/404直败、非法JSON/字段缺失一次纠正、结果+日志+Job+批次同事务提交。
  - `runner.rs`：`AiRuntimeState`（预览注册表+停止信号+取消句柄，不持Key）、启动恢复+持久Runner、并发取当前全局配置。
  - `commands/ai.rs`：`preview_ai_analysis`、`enqueue_ai_analysis`（原子消费、复检变化整批零写入）、`get_ai_analysis`、`list_ai_analysis_summaries`；`lib.rs`注册+启动接线。
  - `logs.rs`最小扩展：新增`sanitized_record`统一脱敏入口，使Runner可在同一事务写日志；`save_log`行为不变。
- 验证结果（2026-08-03，主会话复跑）：
  - `rtk cargo check`：通过，无警告。
  - `rtk cargo test core::ai`：56 passed；4 failed全部为`TcpListener::bind`返回`Operation not permitted`的沙箱环境限制（阶段1同组，非受限环境此前37项全过）。
  - 全量`rtk cargo test`：461 passed（较基线393新增68项），5 failed全部为同类bind环境限制；无其他模块回归。
  - `rtk rustfmt --edition 2021 --check`（全部改动文件）：通过；`rtk git diff --check`：通过。
  - 敏感信息检索：`sk-*`/长Bearer模式在AI模块无命中；日志统一经`SanitizedAiLogRecord`唯一入口。
  - 阻塞段调度加固：`service.rs`把文档收集/哈希复核/配置/钥匙串/提示词构建及所有SQLite写入放入`spawn_blocking`，HTTP请求留在async层；`runner::start`先完成启动恢复再开始领取，避免恢复与领取竞态。
- 已知风险/差异（如实登记）：
  - Windows分支Collector使用保守std检查（symlink_metadata前后比较+canonical根验证），句柄级reparse加固待Windows真机验证；`windows-sys`已声明为直接依赖。
  - HTTP错误映射/重试的端到端测试（401/429/超时/5xx/非法JSON）因沙箱禁绑TCP无法在本环境运行，代码路径与Provider既有安全边界一致，待可绑定端口环境复跑。
  - `auto`输出语言按系统locale解析（zh/zh-TW/en，缺省zh），为未冻结细节的合理假设。
  - `docs/ai-analysis/03-progress.md`由主会话独占更新；`R-009`多子代理通道故障记录保留，待会话恢复后复核。
- 待验收：主会话已检查真实diff（无范围外修改：用户`.gitignore`/`package-lock.json`/`AGENTS.md`未动）；下一步创建全新只读Code Reviewer审查，通过后阶段2标记“已完成”。

阶段2独立审查记录（2026-08-03，主会话，待验收）：

- 尝试创建全新只读Code Reviewer（`phase2_reviewer`）失败：多子代理消息通道（R-009）仍无法送达任务内容，子代理仅返回通用工作区状态。按验收规则，阶段2保持“待验收”，不提前计入进度。
- 主会话执行等价只读自审（未修改文件，审查前后`git status`一致）：逐项核对符号链接/TOCTOU防线（canonical验证+`openat O_NOFOLLOW`逐组件+读取前后`fstat`身份比较）、中心根managed路径精确匹配、预览一次性消费与整批零写入、3次请求预算与成功/取消线性化、启动恢复四步顺序、日志唯一脱敏入口、Key不落盘、四个Command外层`{input}`契约与状态优先级，未发现P0/P1；自审不能替代独立Reviewer，待会话恢复后补做全新只读审查再验收。

阶段2验收结论（2026-08-03，已完成）：

- 用户明确授权“阶段2以主会话自审验收”；多子代理通道故障（R-009）保留为阻塞记录，但不再阻塞本阶段验收。
- 主会话自审结论：未发现P0/P1；`cargo check`零警告、`core::ai` 56 passed（4项为沙箱禁绑TCP环境限制）、全量461 passed（较基线+68，5项同类环境限制）、rustfmt与`git diff --check`通过、敏感信息无命中、无范围外修改。
- 阶段2计入25%，累计进度50%；会话恢复后可补做一次独立只读Reviewer复核，不计入本阶段进度。

委派阻塞记录（2026-08-03）：

- 按多子代理要求派出阶段2实现Agent三次（`phase2_backend_service`、`_b`、`_c`）并用探针验证通道：前两次因“stream disconnected before completion: Encrypted function output content could not be decrypted or decoded”中断且零文件改动；第三次与探针Agent均正常完成但明确回复“未收到具体任务内容”；随后两次`followup_task`（含最小消息“请只回复OK”）同样未送达。
- 已排除任务描述因素：消息从约3000词精简至约500词、去掉模型/推理覆盖、改用不同Agent与不同通道均复现；spawn初始消息与followup消息走同一加密通道，当前会话内该通道疑似整体失效。
- 工作树在全部尝试前后无任何子代理产生的改动，仅主会话登记的进度文档更新；阶段2其余实现未开始，等待用户处置。
