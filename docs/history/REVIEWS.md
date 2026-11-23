# 历次代码评审与维护审计档案

> 归档整理自原 `REVIEW.md`、`ASTRA_REVIEW.md` 及 `docs/HISTORY.md` §2–§3（早期代码基线至 `fad69a1` / v1.4.1）。
> **注意**：本文件属于历史记录。文中代码行号、测试数和“本轮”均对应当时时间点；当前有效架构与约定请见 [`docs/DESIGN.md`](../DESIGN.md)。

## 1. 演进概览与历次评审结论

| 阶段 | 发现与处理 | 证据与边界 |
|---|---|---|
| 早期扫描与评分 | 扫描 JPG/ARW、五维评分、连拍、CSV/XMP 与 SQLite 缓存逐步落地 | 历史里程碑与样本见 [`MILESTONES_AND_DECISIONS.md`](MILESTONES_AND_DECISIONS.md)；现状见 [`docs/DESIGN.md`](../DESIGN.md) |
| 2026-09-08，第 1 轮 | 修复分目录配对、配置静默回退、EXIF 5/7、pose 兜底、缓存指纹和写入；统一 P80 命名与文档 | 当时 `b477c96`，26 → 37 单元 + 6 集成；248 个 pose 置信度均在 (0,1)，只修正 sigmoid 注释 |
| 2026-09-09，第 2、3 轮 | 将 review 统一为主 CLI 子命令；修复 `keep_k` 被默认值覆盖；CSV 补 `burst_pose`；按路径段验证；扫描排除 `.firstcut/`；实现 UI 跑批、改星和配置编辑 | score/review 16 张快照一致；归档 119 张星级分布保持 12/24/41/31/11；当时 51 单元 + 6 集成 |
| 2026-09-18，路线调整 | 停止 darktable 批量 RAW 开发；改为 XMP 星级、筛选与建议曝光交给 Lightroom | 停止方案与实测细节见 [`DARKTABLE_PHASE2_RESEARCH.md`](DARKTABLE_PHASE2_RESEARCH.md)，不继续执行旧开发清单 |
| 2026-09-27，第 4 轮 | 恢复页面脚本、隔离 AI/纯像素缓存、持久化人工星级、保护外部 XMP 字段、修复时间排序/灰度解码/配置边界 | 当时 62 单元 + 7 集成；逐项复现与结果见 §2 |
| v1.2 / v1.3 | 迭代缩放与对比、评分说明、场景反馈、组聚合、可折叠拖动侧栏、时间日志、RGBW 直方图；UI/CLI 统一状态目录与扫描偏好 | v1.3 两入口 272 张、268 张有评分逐项一致；历史实测并非近期重新执行 |
| v1.4.0 | Windows 四程序发行 + Linux amd64/arm64 容器；Apple Silicon 使用 Docker | 原生架构 runner 构建并 HTTP 冒烟；不代表原生 macOS 已验收 |
| 2026-10-01，v1.4.1 | 人工评分状态路径加固；组合升级六项 Dependabot 依赖；PR #7 合入 `main`（`99babbc`），发行提交 `fad69a1` | Windows 72 单元 + 7 集成；Linux 另有 Unix 链接回归；31 条扫描 CSV 与旧版逐字节一致；当时 CI/CodeQL/Release 全通过，原告警 #1 fixed |
| 2026-11-05，文档整理 | 分离当前设计与历史资料，合并评审文件，增加仅本地的沟通摘要忽略规则 | 只调整文档与 Git 忽略规则，不改程序、评分默认值或缓存格式 |
| 2026-11-18，v1.4.2 | 安全加固复核访问、基于句柄的本地存储隔离、导出公式防护、模型哈希校验与容器/依赖审计 | 提交 `e83f366` / `c5555f2`；详情见 [`docs/RELEASE_NOTES.md`](../RELEASE_NOTES.md) |
| 2026-11-22，安全与功能基线审计 | 文档归档整理；修复 `crs:Exposure2012` 与 Adobe 属性 XMP 合并、可移植相对路径缓存与固定模型哈希指纹、前端监听器泄漏/鉴权竞态/跑批组筛选、写操作并发串行与 `MutationGuard`、登录失败限速、Windows 大小写前缀边界与 `firstcut-ui` 重开唤起、CSV 还原与 RAW 连拍覆盖同步 | Windows `cargo test --locked --all-targets` 通过（83 单元 + 7 集成）、`node --check` 通过、`check-review.py` 端到端冒烟通过；详情见 §3.3 |

### 1.1 历次评审的保留结论

- 第 1 轮修复采用“同目录优先、无歧义跨目录兜底”，避免为修复编号回绕而破坏 JPG/RAW 分目录布局。配置错误必须明确退出，不接受静默回退。
- 清晰度仍取 `max(区域分, 全局分)`；改成只用主体区域、letterbox 预处理以及小批次相对星级策略均未擅自实施。
- 第 3 轮建议中的 Host/Origin 校验及报告/配置默认路径已在后续可靠性和一致性修复中落实；旧 `cargo fmt` 建议、已移除二进制的磁盘清理只是当时环境观察，不属于产品功能待办。
- 评审验证过曝光容差带、dHash 严格性、主体几何和 XMP 命名；这些事实不能替代 Lightroom 实际导入或真实选片质量验收。
- 旧流程曾因截断测试输出，只看到 lib 测试通过而漏掉集成失败。以后必须检查完整退出码和测试结果，不能只看开头几行。
- 性能数值、检出率和星级分布均是指定样本/硬件上的历史证据，不承诺对任意目录重复。

## 2. 可靠性与统一数据模型实施记录（原 `ASTRA_REVIEW.md`，2026-09-27）

以下为 2026-09-27 `codex/Astra-review` 评审的实施记录，实现提交为 `f9f60dd`。用户当时确认实施范围为更新文档、恢复可靠性、统一数据模型；真实照片的选片质量校准留到后续轮次。

### 2.1 评审过程与判断

先对照当时的 `DESIGN.md`、`README.md`、`REVIEW.md` 与代码中的扫描、缓存、评分、连拍、XMP、review 路径，区分“当前有效设计”和旧里程碑。对疑点用临时合成照片、侧车和配置做最小复现，再修改共用的数据流，避免只修某个界面的表象。基线测试是 57 项单元测试、6 项集成测试通过；这说明已有回归未覆盖下面的实际故障。

| 复现或观察 | 当时处理 | 验证 |
|---|---|---|
| review 内嵌脚本被换行截断，`node --check` 报 `SyntaxError` | 修复脚本；写接口增加本地 Host 校验 | 脚本检查通过；实际 HTTP 写接口拒绝恶意 Host（403） |
| 共用缓存让纯像素模式命中旧 AI 结果；旧样本缓存美学分 60，AI 重算 39.56 | 缓存键加入分析模式与模型文件指纹，版本升至 15；CSV 标明 `analysis_mode` | 合成样本和缓存回归通过 |
| 人工改星仅反映在旧 XMP，重启后快照重新给出算法星级（复现为侧车 1 星、快照 5 星） | 人工星级放入照片根目录 `.firstcut/decisions.sqlite`；由 `selection` 统一合成 CLI、review、CSV、XMP 的最终星级 | 改星、重跑、重启后仍保留人工值，集成测试覆盖导出 |
| 已有 firstcut 侧车重写时丢失外部 XML 字段 | 按 XML 节点合并管理字段；解析失败时保护原文件；临时文件写入后替换 | 未知字段保留与异常文件回归通过 |
| 文件路径顺序导致拍摄时间倒退仍被当作同一连拍；单通道 JPEG 触发越界 panic | 按 EXIF 时间排序且不接受负时间差；灰度 JPEG 走安全解码路径 | 对应回归通过 |
| 负权重、NaN、零曲线参数、越界百分位未被拒绝；sports 清晰度参数方向与公式相反 | 补参数有限性与范围校验；修正 sports 预设与说明 | 配置边界回归通过 |

最终验证为 **62 项单元测试和 7 项集成测试通过**，前端脚本检查通过，并用本地 HTTP 请求核对了人工星级重启持久化和 Host 拒绝行为。此轮没有对真实照片的选片准确度、Lightroom 真实导入和曝光建议效果作验收；测试结果不能替代这些人工检查。

### 2.2 统一后的有效数据模型

[`docs/DESIGN.md` §2 统一数据模型](../DESIGN.md#2-统一数据模型) 确立了现行规则：照片文件是资产；分析缓存可重建，只保存五维指标等分析结果，并由模式、配置曲线、模型文件信息区分来源；每次批次重新合成算法总分、相对星级和连拍建议；人工决定在独立 SQLite 中持久化，最终星级采用人工优先。CLI 和 review 共用 `selection::SelectionResult`。CSV 展示 `analysis_mode`、`rating_source`，XMP 是导出结果而非人工决定的唯一存储。备份照片时，应连同 `.firstcut/decisions.sqlite` 备份；分析缓存可重新生成。

### 2.3 当时归档清理记录

- `M5_REVIEW.md` 的 A/B/C 决策、模型和权重选择已归入 [`MILESTONES_AND_DECISIONS.md`](MILESTONES_AND_DECISIONS.md) 和现行设计文档；原文件中特有的夜景排名、119 张旧星级分布及备选方案亦保留在该归档中。
- `P2_M0.md` 的镜头普查、lensfun 修正、neural restore 的 CLI 限制、XMP/EV/16bit TIFF 冒烟和 darktable-cli 命令坑已归入 [`DARKTABLE_PHASE2_RESEARCH.md`](DARKTABLE_PHASE2_RESEARCH.md)。其最初 2048px JPG 单张约 2.6 秒的早期实验，后来由完整批处理基准（约 3.7 秒/张）取代。原清单里未勾选的 GUI 验证项已随该路线停止，不是当前待办。

### 2.4 当时记录的下一轮建议

1. 用用户真实选片结果建立小规模标注集，先看好片被排除率、每组连拍最佳帧是否仍在候选内，再调整权重或阈值。此前样本分布不足以判断真实选片质量；该轮按约定没有改选片算法。
2. 用 Lightroom 实际导入一组 JPG/ARW 与 XMP，核对星级、配对和 `crs:Exposure2` 的显示及效果。现有自动测试只能证明侧车字段和文件写入，不能证明 Lightroom 的实际行为。
3. 在扩展人工“保留/排除”等操作前，把新决定存入独立决定库并经过同一 `selection` 合成路径，保持 CLI、review 与导出一致。

## 3. 后续维护审计记录

### 3.1 UI 与 CLI 统一及 v1.3.0 发布（2026-09-29）

用户在人肉验证灯箱缩放显示后，发现双击启动的 UI 与 CLI 对已有评分目录的判定可能不同。核对默认路径后确认：UI 一直将缓存放在所选照片根 `.firstcut/cache.sqlite`，旧 CLI 默认将缓存写到进程工作目录的 `pic_process_cache.sqlite`；同时 CLI 的 RAW 默认值和 `Processed/` 扫描行为与 UI 不同。启动方式变化或工作目录变化会造成缓存未命中和图库内容不一致。

该次将缓存、报告、配置和扫描输出默认统一到照片根 `.firstcut/`；CLI `score` 与 `review` 读取同一 RAW 偏好，均跳过 `Processed/`；UI 与 CLI 复核使用相同的照片根路径规范化。`tune` 与 `gallery` 的默认报告也放在对应照片根或输入报告目录的 `.firstcut/`。CLI 显式传入的路径仍可覆盖默认值。旧版 CWD 缓存不自动猜测或迁移，需通过 `--cache` 显式指定，避免错误导入另一图库的数据。除面向 Lightroom 的 XMP 侧车外，firstcut 默认内部状态都收纳在 `.firstcut/`。

验证时用双击 UI 启动器和 CLI review 同时读取同一照片根目录，两边均发现 272 张照片，其中 268 张有评分；API 快照逐张比较了路径、RAW 配对、星级来源、总分和连拍保留建议，结果完全一致。代码回归验证 `cargo test --locked --all-targets` 通过（72 单元、7 集成），`cargo build --locked --bin pic_process` 和 `git diff --check` 通过。Release 发布前仍由 GitHub Actions 再跑一次相同测试与 Windows 构建。

该次文档同时注明当前只按 SDR JPEG 进行分析和直方图统计，HDR 增益图、PQ/HLG 尚未支持；收录缩放百分比、EXIF 单位、RGBW 直方图和 10% 默认网格抽样等界面变化。v1.3.0 已推送 GitHub 并通过 CI 与 Release 工作流，发布页提供四个 Windows 程序和 SHA-256 校验文件。真实照片选片质量仍需要用户独立验收。

### 3.2 路径告警与依赖兼容性维护（2026-10-01，v1.4.1）

CodeQL 路径告警涉及复核界面选择的照片根目录传入人工评分数据库。程序允许用户选择本地照片目录，但固定子路径也可能因现有 `.firstcut` 或数据库符号链接被重定向。该次规范化根目录，拒绝符号链接或非目录形式的状态目录，检查规范化后的状态目录仍位于照片根目录内，并拒绝数据库符号链接；Unix 回归用例覆盖状态目录与数据库链接两种情况。CodeQL 对合法的用户目录选择产生的重复告警已附理由标为误报，扫描工作流继续启用。

按用户确认，对 Dependabot 的六项升级作组合验证：clap 4.6.7、kamadak-exif 0.6.1、roxmltree 0.21.1、toml 1.1.6、toml_edit 0.25.15，以及 actions/cache v6。XML 新版改变了部分属性查询的匹配规则，但现有 XMP 代码明确检查命名空间；EXIF 新版移除的数值转换 API 未被项目使用；TOML 新版的配置读写与注释保留由现有回归覆盖。两个 TOML 升级对锁文件的重叠修改已按最终依赖集合重新解析，并保留其他依赖版本。

本地 `cargo test --locked --all-targets` 通过（72 单元、7 集成），全部场景预设、配置编辑和 XMP 保留测试通过。升级前后程序扫描同一组本地样本，31 条 CSV 记录逐字节一致；比较输出只临时保存于系统临时目录，完成后删除。合入前要求组合版本的 Windows、Linux x64、Linux ARM64 CI 和 CodeQL 检查全部通过。普通照片目录无需迁移缓存、人工决定或配置；使用符号链接存放人工评分状态的目录将被拒绝。

组合版本通过全部 PR 检查后，经 PR #7 合入 `main`（`99babbc`）；六个 Dependabot PR 均自动标记为已合并。主分支 CI 和 CodeQL 也全部通过，原告警 #1 状态为 fixed，开放扫描告警为 0。按用户确认，将包版本与锁文件版本更新为 1.4.1，发行标签使用 `v1.4.1`；发行工作流交付四个 Windows x64 程序及 SHA-256 文件，并发布 Linux amd64/ARM64 容器。

### 3.3 安全与功能基线审计修复（2026-11-22，基线 `c5555f2` / v1.4.2）

在全量源码静态审查中发现并集中修复以下问题（未引入新依赖、`Cargo.lock` 与 `CACHE_VERSION = 15` 保持不变）：

| 类别 | 问题 | 修复与回归覆盖 |
|---|---|---|
| XMP 交接 | 曝光建议写入了不存在的 `crs:Exposure2` 且缺少 `crs:HasSettings`/`crs:ProcessVersion`；`merge_xmp` 不兼容 Adobe XMP SDK 规范化后的属性及自闭合标签 | 改为 `crs:Exposure2012` + `crs:HasSettings="True"` + `crs:ProcessVersion="11.0"`（不降级已有更新版本）；支持属性与元素双形态合并、自动清理旧版 `crs:Exposure2` 并在存在外部 CRS 调整时保留 `HasSettings`/`ProcessVersion`（新增 `merge_handles_adobe_attribute_form_and_preserves_external_crs`） |
| 缓存可移植性 | `cache.sqlite` 以绝对路径为键且 `analysis_fingerprint` 混入本机模型 `mtime`，导致目录搬迁与跨机/Docker 复核缓存全部失效，无本地模型时无法只读浏览已有 AI 评分 | 写入改用相对路径键与固定官方模型 SHA-256 指纹；保留旧版绝对路径键与本机验证模型 `mtime` 指纹只读回退，再次评分无痛迁移（新增 `cache_key_is_relative_and_lookup_falls_back_to_legacy_absolute_path` 与 `portable_ai_cache_and_manual_burst_override_propagate_to_paired_raw`） |
| 前端可靠性 | `bindPane()` 每次切换照片向 `window` 泄漏 `mousemove`/`mouseup` 监听器；`setStars()` 触发整页灯箱重拉原图；`loadHistogramSettings`/`pollJob` 早于 `#access=` 登录导致 401；跑批刷新未更新 `#fGroup` 选项 | 改用 `.pane` 局部 Pointer Events + `setPointerCapture` 与 `touch-action:none`；改星仅调用 `renderLBActive()`；将设置读取与任务轮询移至登录成功之后；抽出 `rebuildGroupOptions()` 在跑批刷新时重建组列表 |
| 并发与防阻塞 | 快速连点改星触发 `decisions.sqlite` 非阻塞 `try_lock()` 报 500 或乱序写 XMP；写操作与 `score_run`/关闭服务存在 TOCTOU 竞态；磁盘 `fsync` 直接阻塞 Tokio worker | `JobState` 增加 RAII `MutationGuard` 与 `begin()`/`begin_shutdown()` 互斥；`AppState` 增加异步 `write_lock` 并将写盘与 SQLite/XMP 操作迁入 `spawn_blocking`（新增 `mutation_guard_blocks_scoring_and_shutdown_until_dropped` 与 `concurrent_rate_requests_serialize_without_lock_errors`） |
| 鉴权与存储边界 | `POST /api/session` 缺失败限速；Windows `FileRef::at` 大小写敏感前缀可能绕过相册无链接检查；`FIRSTCUT_MODEL_DIR` 含软链祖先路径时静默降级；`firstcut-ui.exe` 二次双击在清缓存后无法找回凭据 | `sign_in` 增加 60s 内最多 5 次失败限速（返回 `429` + `Retry-After`）；Windows `strip_album_prefix` 改大小写不敏感；模型目录加载前规范化祖先路径并继续逐文件无链接 SHA-256 校验；`firstcut-ui` 通过当前会话命名事件 `Local\firstcut-ui-reopen-8787` 唤起已有进程重开带凭据链接（新增 `sign_in_throttles_repeated_failures`、`windows_strip_album_prefix_is_case_insensitive`） |
| 数据一致性 | `scan::load_report_exif` 与 `gallery` 读取表格保护后的 `report.csv` 未还原前置 `'`；`burst-overrides.json` 重启后未同步到配对 RAW | 增加 `csv::unescape_spreadsheet_text` 并还原 EXIF/画廊文本；快照构建按 `pair_id` 将手动连拍覆盖同步到配对 RAW |

**验证记录（Windows x64）**：
- `cargo test --locked --all-targets`：退出码 0，**83 项单元测试 + 7 项集成测试全通过**（含本地 `testpic/` 扫描用例）。
- `cargo build --locked --bin pic_process --bin firstcut-ui --bin pic_process-gallery --bin pic_process-tune`：退出码 0，无编译警告。
- `node --check src/review/assets/review.js`：退出码 0。
- `python .github/scripts/check-review.py --binary target/debug/pic_process.exe`：退出码 0（认证复核、原图/缩略图、重评分、落盘全通过）。
- 未覆盖范围：仍未执行 Lightroom / Adobe Camera Raw 真机导入验收、真实照片选片质量标注评估，以及 Linux/macOS 容器真机构建（交由 PR CI 验证）；`storage.rs` 仍严格保持拒绝硬链接（`nlink == 1`）与 Windows 重解析点（含 OneDrive 云占位目录）策略。

### 3.4 PR #14 复审意见修复（2026-11-23，基于 `caf16c8`）

针对 PR #14 外部复审指出的 2 项 P1 与 2 项 P2 问题完成修复（按用户确认的方案 A 保护 Lightroom 人工曝光，`Cargo.toml`/`Cargo.lock` 与 `CACHE_VERSION = 15` 保持不变）：

| 级别 | 问题 | 修复与回归覆盖 |
|---|---|---|
| P1 | 仅改星或常规重评分会覆盖已有 Lightroom `crs:Exposure2012`，建议曝光为空时甚至删除原曝光字段 | 引入 `XmpWriteMode`（`MetadataOnly` / `ApplySuggestedExposure` / `RatingOnly`）：`POST /api/rate` 对已有 firstcut 侧车仅更新 `xmp:Rating`；常规 `score --xmp` 与 UI 默认跑批仅写信息字段 `firstcut:suggestedEV`；写入 `crs:Exposure2012` 改为显式开启（`--xmp-exposure`、`[metric] exposure_write_crs = true` 或 UI 复选框 `#runXmpExposure`，默认关闭），且**绝不覆盖或删除已有的 `crs:Exposure2012` / `crs:Exposure`**（新增 `existing_lightroom_exposure_is_never_overwritten_or_deleted`、`rate_preserves_existing_lightroom_exposure_on_disk`、`xmp_exposure_opt_in_and_manual_exposure_protection`） |
| P1 | HTTP 请求在等待 `spawn_blocking` 落盘期间被取消时，挂在 handler 栈上的 `write_lock` 与 `MutationGuard` 提前释放，后台写入继续而内存/快照更新被跳过 | 新增取消安全的 `run_serialized_mutation`，将 `Arc::clone(&state.write_lock).lock_owned().await`、`MutationGuard` 与 `AppState` 一并移入 `spawn_blocking`，所有写接口（改星、连拍保留、场景反馈、配置保存、界面设置）统一在阻塞任务内完成落盘、内存状态与快照更新后才释放双守卫（新增 `cancelled_mutation_keeps_locks_and_updates_snapshot_before_next_write_or_job`） |
| P2 | 同一照片同时存在相对路径记录与旧版绝对路径记录时，失效的相对路径记录会挡住仍有效的旧记录，导致重算或显示未评分 | 用 `cache::find_matching` 替代 `cache::lookup`，对相对路径键与旧版绝对路径键分别按指纹条件校验；同步在 `docs/DESIGN.md` 补充“旧库需先在原路径跑批迁移”及“搬迁照片须保留 `mtime`”说明（新增 `stale_relative_cache_row_does_not_shadow_valid_legacy_absolute_row`） |
| P2 | UI 重评分后原先选中的连拍组消失时，`loadData()` 先 `apply()` 后 `rebuildGroupOptions()` 留下空白网格 | 将 `rebuildGroupOptions()` 提前至 `ingest()` 中 `apply()` 之前，并在当前选中组失效时重置 `f.group = "all"` 与 `page = 0` 再渲染 |

**验证记录（Windows x64）**：
- `cargo test --locked --all-targets`：退出码 0，**86 项单元测试 + 8 项集成测试全通过**（无编译警告）。
- `cargo build --locked --bin pic_process --bin firstcut-ui --bin pic_process-gallery --bin pic_process-tune`：退出码 0，无编译警告。
- `node --check src/review/assets/review.js`：退出码 0。
- `python .github/scripts/check-review.py --binary target/debug/pic_process.exe`：退出码 0（认证复核、原图/缩略图、重评分、落盘全通过）。
- 未覆盖范围：同 3.3（真实 Lightroom / ACR 导入验收、真实选片质量评估、Linux/ARM64 容器真机构建交由 PR CI 验证）。
