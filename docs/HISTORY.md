# 项目历史、评审与决策档案

> 整理日期：2026-11-05；所依据的代码基线：`fad69a1`（v1.4.1）。
> 当前有效约定统一见 [DESIGN.md](../DESIGN.md)。本文件解释过去为什么这样做；历史代码行号、测试数量、默认值和未勾选任务均只适用于当时版本，不是当前实施清单。

## 1. 文档合并与阅读方式

本文件合并原 `DESIGN.md` 的旧里程碑、停止的 darktable 方案与决策表，以及 `REVIEW.md`、`ASTRA_REVIEW.md` 的评审结论和实施证据。两份独立评审文件已移除。重复的修复草图、逐条旧验收清单和过期操作命令不再作为日常文档维护；原文保存在 Git 历史，例如 `git show fad69a1:REVIEW.md`、`git show fad69a1:ASTRA_REVIEW.md` 和 `git show fad69a1:DESIGN.md`。更早的 `M5_REVIEW.md`、`P2_M0.md` 已在此前合并清理，其独有实验记录仍在下方历史章节中。

[README](../README.md) 负责使用说明，[设计文档](../DESIGN.md) 负责当前约定，[版本说明](../release_notes.md) 记录用户可见变化，[安全政策](../SECURITY.md) 负责漏洞报告。后续评审统一追加到本文件，涉及仍有效的问题应同时更新设计文档的待办表。

## 2. 演进概览与评审结论

| 阶段 | 发现与处理 | 证据与边界 |
|---|---|---|
| 早期扫描与评分 | 扫描 JPG/ARW、五维评分、连拍、CSV/XMP 与 SQLite 缓存逐步落地 | 历史里程碑与样本见 §4；模型与评分现状见设计文档 |
| 2026-09-08，第 1 轮 | 修复分目录配对、配置静默回退、EXIF 5/7、pose 兜底、缓存指纹和写入；统一 P80 命名与文档 | 当时 `b477c96`，26 → 37 单元 + 6 集成；248 个 pose 置信度均在 (0,1)，只修正 sigmoid 注释 |
| 2026-09-09，第 2、3 轮 | 将 review 统一为主 CLI 子命令；修复 `keep_k` 被默认值覆盖；CSV 补 `burst_pose`；按路径段验证；扫描排除 `.firstcut/`；实现 UI 跑批、改星和配置编辑 | score/review 16 张快照一致；归档 119 张星级分布保持 12/24/41/31/11；当时 51 单元 + 6 集成 |
| 2026-09-18，路线调整 | 停止 darktable 批量 RAW 开发；改为 XMP 星级、筛选与建议曝光交给 Lightroom | 停止方案与实测细节见 §5，不继续执行旧开发清单 |
| 2026-09-27，第 4 轮 | 恢复页面脚本、隔离 AI/纯像素缓存、持久化人工星级、保护外部 XMP 字段、修复时间排序/灰度解码/配置边界 | 当时 62 单元 + 7 集成；逐项复现与结果见 §3 |
| v1.2 / v1.3 | 迭代缩放与对比、评分说明、场景反馈、组聚合、可折叠拖动侧栏、时间日志、RGBW 直方图；UI/CLI 统一状态目录与扫描偏好 | v1.3 两入口 272 张、268 张有评分逐项一致；历史实测并非本次重新执行 |
| v1.4.0 | Windows 四程序发行 + Linux amd64/arm64 容器；Apple Silicon 使用 Docker | 原生架构 runner 构建并 HTTP 冒烟；不代表原生 macOS 已验收 |
| 2026-10-01，v1.4.1 | 人工评分状态路径加固；组合升级六项 Dependabot 依赖；PR #7 合入 `main`（`99babbc`），发行提交 `fad69a1` | Windows 72 单元 + 7 集成；Linux 另有 Unix 链接回归；31 条扫描 CSV 与旧版逐字节一致；当时 CI/CodeQL/Release 全通过，原告警 #1 fixed |
| 2026-11-05，文档整理 | 分离当前设计与历史资料，合并评审文件，增加仅本地的沟通摘要 | 只调整文档与 Git 忽略规则，不改程序、评分默认值或缓存格式 |

### 2.1 历次评审的保留结论

- 第 1 轮修复采用“同目录优先、无歧义跨目录兜底”，避免为修复编号回绕而破坏 JPG/RAW 分目录布局。配置错误必须明确退出，不接受静默回退。
- 清晰度仍取 `max(区域分, 全局分)`；改成只用主体区域、letterbox 预处理以及小批次相对星级策略均未擅自实施。
- 第 3 轮建议中的 Host/Origin 校验及报告/配置默认路径已在后续可靠性和一致性修复中落实；旧 `cargo fmt` 建议、已移除二进制的磁盘清理只是当时环境观察，不属于产品功能待办。
- 评审验证过曝光容差带、dHash 严格性、主体几何和 XMP 命名；这些事实不能替代 Lightroom 实际导入或真实选片质量验收。
- 旧流程曾因截断测试输出，只看到 lib 测试通过而漏掉集成失败。以后必须检查完整退出码和测试结果，不能只看开头几行。
- 性能数值、检出率和星级分布均是指定样本/硬件上的历史证据，不承诺对任意目录重复。

## 3. 可靠性与统一数据模型的实施记录

以下合并原 `ASTRA_REVIEW.md` 的实施记录，所称“本轮”指 2026-09-27 的 `codex/Astra-review` 评审，而非本次文档整理。用户当时确认实施范围为更新文档、恢复可靠性、统一数据模型；真实照片的选片质量校准留到下一轮。实现提交为 `f9f60dd`。

### 评审过程与判断

先对照 `DESIGN.md`、`README.md`、`REVIEW.md` 与代码中的扫描、缓存、评分、连拍、XMP、review 路径，区分“当前有效设计”和旧里程碑。对疑点用临时合成照片、侧车和配置做最小复现，再修改共用的数据流，避免只修某个界面的表象。基线测试是 57 项单元测试、6 项集成测试通过；这说明已有回归未覆盖下面的实际故障。

| 复现或观察 | 本轮处理 | 验证 |
|---|---|---|
| review 内嵌脚本被换行截断，`node --check` 报 `SyntaxError` | 修复脚本；写接口增加本地 Host 校验 | 脚本检查通过；实际 HTTP 写接口拒绝恶意 Host（403） |
| 共用缓存让纯像素模式命中旧 AI 结果；旧样本缓存美学分 60，AI 重算 39.56 | 缓存键加入分析模式与模型文件指纹，版本升至 15；CSV 标明 `analysis_mode` | 合成样本和缓存回归通过 |
| 人工改星仅反映在旧 XMP，重启后快照重新给出算法星级（复现为侧车 1 星、快照 5 星） | 人工星级放入照片根目录 `.firstcut/decisions.sqlite`；由 `selection` 统一合成 CLI、review、CSV、XMP 的最终星级 | 改星、重跑、重启后仍保留人工值，集成测试覆盖导出 |
| 已有 firstcut 侧车重写时丢失外部 XML 字段 | 按 XML 节点合并管理字段；解析失败时保护原文件；临时文件写入后替换 | 未知字段保留与异常文件回归通过 |
| 文件路径顺序导致拍摄时间倒退仍被当作同一连拍；单通道 JPEG 触发越界 panic | 按 EXIF 时间排序且不接受负时间差；灰度 JPEG 走安全解码路径 | 对应回归通过 |
| 负权重、NaN、零曲线参数、越界百分位未被拒绝；sports 清晰度参数方向与公式相反 | 补参数有限性与范围校验；修正 sports 预设与说明 | 配置边界回归通过 |

最终验证为 **62 项单元测试和 7 项集成测试通过**，前端脚本检查通过，并用本地 HTTP 请求核对了人工星级重启持久化和 Host 拒绝行为。此轮没有对真实照片的选片准确度、Lightroom 真实导入和曝光建议效果作验收；测试结果不能替代这些人工检查。

### 统一后的有效数据模型

[当前设计的数据模型](../DESIGN.md#2-统一数据模型) 是当前规则：照片文件是资产；分析缓存可重建，只保存五维指标等分析结果，并由模式、配置曲线、模型文件信息区分来源；每次批次重新合成算法总分、相对星级和连拍建议；人工决定在独立 SQLite 中持久化，最终星级采用人工优先。CLI 和 review 共用 `selection::SelectionResult`。CSV 展示 `analysis_mode`、`rating_source`，XMP 是导出结果而非人工决定的唯一存储。备份照片时，应连同 `.firstcut/decisions.sqlite` 备份；分析缓存可重新生成。

[README](../README.md) 提供操作说明，[版本说明](../release_notes.md) 记录发行变化，本文件统一保存历次评审。旧里程碑的实施设想不能覆盖当前约定：darktable 批量 RAW 开发已于 2026-09-18 停止，产品聚焦初筛、复核和给 Lightroom 的 XMP 星级/曝光建议。

### 历史资料归档与清理

`M5_REVIEW.md` 的 A/B/C 决策、模型和权重选择已归入本文件 §4 和当前设计的评分章节；原文件中特有的夜景排名、119 张旧星级分布及备选方案也保留在本文件 §4。那些数字只表示 M5 当时样本，当前默认星级为批次相对分档。

`P2_M0.md` 的镜头普查、lensfun 修正、neural restore 的 CLI 限制、XMP/EV/16bit TIFF 冒烟和 darktable-cli 命令坑已归入本文件 §5（原设计 §9.3–§9.4d）；其最初 2048px JPG 单张约 2.6 秒的早期实验，后来由原 §9.4c 的完整批处理基准（约 3.7 秒/张）取代。原清单里未勾选的 GUI 验证项已随该路线停止，不是当前待办。两个旧文件从工作树移除，原文仍可从 Git 历史读取。

### 下一轮建议与原因

1. 用用户真实选片结果建立小规模标注集，先看好片被排除率、每组连拍最佳帧是否仍在候选内，再调整权重或阈值。此前样本分布不足以判断真实选片质量；本轮按约定没有改选片算法。
2. 用 Lightroom 实际导入一组 JPG/ARW 与 XMP，核对星级、配对和 `crs:Exposure2` 的显示及效果。现有自动测试只能证明侧车字段和文件写入，不能证明 Lightroom 的实际行为。
3. 在扩展人工“保留/排除”等操作前，把新决定存入独立决定库并经过同一 `selection` 合成路径，保持 CLI、review 与导出一致。

### 后续维护：UI 与 CLI 统一及 v1.3.0 发布（2026-09-29）

用户在人肉验证灯箱缩放显示后，发现双击启动的 UI 与 CLI 对已有评分目录的判定可能不同。核对默认路径后确认：UI 一直将缓存放在所选照片根 `.firstcut/cache.sqlite`，旧 CLI 默认将缓存写到进程工作目录的 `pic_process_cache.sqlite`；同时 CLI 的 RAW 默认值和 `Processed/` 扫描行为与 UI 不同。启动方式变化或工作目录变化会造成缓存未命中和图库内容不一致。

本次将缓存、报告、配置和扫描输出默认统一到照片根 `.firstcut/`；CLI `score` 与 `review` 读取同一 RAW 偏好，均跳过 `Processed/`；UI 与 CLI 复核使用相同的照片根路径规范化。`tune` 与 `gallery` 的默认报告也放在对应照片根或输入报告目录的 `.firstcut/`。CLI 显式传入的路径仍可覆盖默认值。旧版 CWD 缓存不自动猜测或迁移，需通过 `--cache` 显式指定，避免错误导入另一图库的数据。除面向 Lightroom 的 XMP 侧车外，firstcut 默认内部状态都收纳在 `.firstcut/`。

验证时用双击 UI 启动器和 CLI review 同时读取同一照片根目录，两边均发现 272 张照片，其中 268 张有评分；API 快照逐张比较了路径、RAW 配对、星级来源、总分和连拍保留建议，结果完全一致。代码回归验证 `cargo test --locked --all-targets` 通过（72 单元、7 集成），`cargo build --locked --bin pic_process` 和 `git diff --check` 通过。Release 发布前仍由 GitHub Actions 再跑一次相同测试与 Windows 构建。

此次文档同时注明当前只按 SDR JPEG 进行分析和直方图统计，HDR 增益图、PQ/HLG 尚未支持；收录缩放百分比、EXIF 单位、RGBW 直方图和 10% 默认网格抽样等近期界面变化。v1.3.0 已推送 GitHub 并通过 CI 与 Release 工作流，发布页提供四个 Windows 程序和 SHA-256 校验文件。真实照片选片质量仍需要用户独立验收，不在本轮推断算法质量。

### 后续维护：路径告警与依赖兼容性（2026-10-01）

CodeQL 路径告警涉及复核界面选择的照片根目录传入人工评分数据库。程序允许用户选择本地照片目录，但固定子路径也可能因现有 `.firstcut` 或数据库符号链接被重定向。本次规范化根目录，拒绝符号链接或非目录形式的状态目录，检查规范化后的状态目录仍位于照片根目录内，并拒绝数据库符号链接；Unix 回归用例覆盖状态目录与数据库链接两种情况。CodeQL 对合法的用户目录选择产生的重复告警已附理由标为误报，扫描工作流继续启用。

按用户确认，对 Dependabot 的六项升级作组合验证：clap 4.6.7、kamadak-exif 0.6.1、roxmltree 0.21.1、toml 1.1.6、toml_edit 0.25.15，以及 actions/cache v6。XML 新版改变了部分属性查询的匹配规则，但现有 XMP 代码明确检查命名空间；EXIF 新版移除的数值转换 API 未被项目使用；TOML 新版的配置读写与注释保留由现有回归覆盖。两个 TOML 升级对锁文件的重叠修改已按最终依赖集合重新解析，并保留其他依赖版本。

本地 `cargo test --locked --all-targets` 通过（72 单元、7 集成），全部场景预设、配置编辑和 XMP 保留测试通过。升级前后程序扫描同一组本地样本，31 条 CSV 记录逐字节一致；比较输出只临时保存于系统临时目录，完成后删除。合入前要求组合版本的 Windows、Linux x64、Linux ARM64 CI 和 CodeQL 检查全部通过。普通照片目录无需迁移缓存、人工决定或配置；使用符号链接存放人工评分状态的目录将被拒绝。

组合版本通过全部 PR 检查后，经 PR #7 合入 `main`（`99babbc`）；六个 Dependabot PR 均自动标记为已合并。主分支 CI 和 CodeQL 也全部通过，原告警 #1 状态为 fixed，开放扫描告警为 0。按用户确认，将包版本与锁文件版本更新为 1.4.1，发行标签使用 `v1.4.1`；发行工作流交付四个 Windows x64 程序及 SHA-256 文件，并发布 Linux amd64/ARM64 容器。

## 4. 早期里程碑与模型选择（原设计 §7）

以下保留旧阶段标签和原设计的 §7/§10/§12 引用，查原稿可用 `git show fad69a1:DESIGN.md`。其中“只读”“待办”等只描述当时阶段，当前实现以设计文档为准。

- ✅ **M0 骨架**：CLI + 目录扫描 + EXIF + CSV 输出（268 张真实照片实测通过）
- ✅ **M1 像素指标**：清晰度/曝光/噪点 + 加权总分（纯算法，无 AI 依赖）
- ✅ **M2 连拍去重**：时间聚类 + dHash 子簇聚类 + 组内排序 top-K（6 项单元测试）
- ✅ **M3 AI 接入**：CLIPIQA 美学 + SCRFD 人脸检测 + 构图维度，5 维评分（后期从 MUSIQ/YuNet 升级，见 M5）
- ✅ **M4 输出完善**：XMP 星级侧车（`<名>.<原扩展名>.xmp`，xmp:Rating + firstcut 子分，他人侧车保护）+ SQLite 增量缓存（size+mtime+版本键，二次运行 16/16 命中 0.78s；M6 追加配置指纹）
- ✅ **M5 调参与验证**：性能优化（21.5s→10.6s/119张）、噪点 P15 修复、浅景深清晰度误判修复（主体感知三层链路：SCRFD 人脸区域 reblur → YOLOv8-pose 头部区域 → 50 中性下限）、人脸漏检换 SCRFD（检出 23→75/119）、gallery 联系表；**用户决策落地**：A=多场景配置文件（--config/config-template，权重+曲线+星级+曝光目标可配）、B=星级放宽 75/60/45/30、C=换 CLIPIQA（美学分布 26-76，区分度提升）
- ✅ **M6 缺陷修复**（2026-07，见 §10 决策记录）：AI 预处理通道顺序 bug、EXIF 方向未处理、构图被贴纸脸/背景脸污染、曝光模型改为 EV 容差带 + 主体感知单向修正、缓存键缺配置指纹、场景预设落地
- 🔄 **M7 交付与兼容**（2026-07，见 §10）：XMP 侧车命名改 Lightroom 约定、星级改批次内相对分档、配对键含目录、连拍排序用实际权重；**待办**：LR 真实导入验证、自适应连拍保留、ground truth 校准
- ✅ **M8 评审修复**（2026-07，见 §10）：外部评审（GLM）13 项问题逐条核实并修复——配对回归（P0）、配置 fail-fast、EXIF 5/7 互换、清晰度兜底缝隙、指纹收窄、缓存 flush 收窄、文档批次修正、pose sigmoid 定案
- ✅ **M9 自适应连拍保留**（2026-09，见 §7.1 / §10）：SCRFD kps 解码 + 姿态描述子缓存化 + dHash 子簇内姿态聚类（每姿势簇保留 3 张、单组上限 20、默认启用可关闭）；阈值 0.25 经棚拍双峰定标；archive 119 张回归星级逐档一致（12/24/41/31/11）；搭车：`analysis_ok` 列、实验性 `--gpu`（DirectML，无显著收益）
- ✅ **M-UI1 复核界面**（2026-09，见 §12）：`pic_process review <目录>` 本地 Web 界面——缩略图墙 + 筛选排序 + 1:1 原图灯箱 + 连拍组并排同步缩放对比；只读
- 🔄 **M7 遗留待办**：LR 真实导入验证、ground truth 校准

**M5 决策的历史边界**：当时夜景配置使样片 DSC00898 排名从 #51 升到 #7；绝对星级阈值 75/60/45/30 在当时的 119 张样本上得到 5 星 24 张、4 星 74 张、3 星 21 张。A/B/C 的备选曾分别是“提高曝光权重或保持原样”“保持或放宽阈值”“继续 MUSIQ、多尺度或换模型”，最终选择多场景可配置、放宽阈值与 CLIPIQA+。这些是 M5 样本和决策过程，**当前默认星级已经改为 M7 的批次相对分档**；重调参数应使用新的人工选片标注，不把旧分布当验收基线。浅景深验证样片还包括 DSC00893（人脸）和 DSC00974/976（人体定位）。

### 7.1 M9 自适应连拍保留（已实现）：按姿态自适应保留连拍

**问题**：`keep_k` 固定、按 dHash 子簇保留。30fps 连拍时同一个人的**不同姿势/表情**会被 dHash 判为近似重复，只留 2 张 → 丢掉不同瞬间。

**实现**（利用已有算力，零额外推理成本）：
1. SCRFD `scrfd_10g_bnkps` 的 **5 个关键点（双眼/鼻/双嘴角）**在 M9 起参与解码（kps 张量 [N,10]，格中心 + offset×stride，与 bbox 同一距离约定；debug_scrfd 实测 40 点 100% 落框内自检）。按人脸框归一化成 10 维**姿态描述子**（`score::pose_descriptor`），对位置/尺度不变。
2. 描述子进缓存（`pose_desc BLOB`，`CACHE_VERSION` 13）。
3. dHash 子簇内**贪心种子姿态聚类**（与 dHash 聚类同风格、扫描顺序确定）：距离 > 0.25 → 新姿势簇；无描述子帧共享伪簇。
4. 每个**姿势簇**各自保留 top-K（默认 3）；单组保留总量上限 20（超出按总分截断）；`adaptive_keep=false` 一键回退 M2 行为（逐字节一致）。

**定标**：棚拍 18 帧同主体连拍——同姿势两两距离 0.02~0.06、跨姿势 0.46~0.51，双峰清晰，0.25 居中（两侧余量 4×+）；舞台连拍（萤火虫）姿态连续变化时无真空带，0.25 给出合理"瞬间"粒度。

**规模实测**：真实图库 1447 张（88% 在连拍组）——姿态簇规模分布 1~9 张、组保留数 ≤ 20，行为符合预期；M9 只改 burst 列，五维分与星级与 M8 完全一致（回归断言）。

> 当时状态（Phase 1 + M6/M7/M8/M9 + M-UI1）：`pic_process score <目录> [--xmp] [--config x.toml] [--cache <文件>] [--no-ai] [-k N] [--gpu]`，42 项单元测试 + 6 项集成测试全过；冷跑 ~121-146ms/张（含 SCRFD+姿态，16 核）。

### 4.2 模型选择与限制的历史证据（原设计 §8）

> 此处的“当前”是原稿所处阶段；保留模型分布、样本检出率和性能取舍，不代表重新实测。

- **美学模型**：~~MUSIQ~~ → **已定案：CLIPIQA+ ONNX**（M5 切换，86Cao/IQA-ONNX-Models，224×224、CLIP 归一化、sigmoid×100，分布区分度 26-76 高于 MUSIQ 的 36-65；已下载至 `models/clipiqa_model.onnx{,.data}`）。
- **人脸检测**：~~YuNet~~ → **已定案：SCRFD 10g**（M5 切换，RuteNL/SCRFD-face-detection-ONNX，640×640、9 输出解码、阈值 0.3 + 贪心 NMS；119 张真实照片检出 23→75，M6 修 EXIF 方向后 109/119）。
- **onnxruntime Windows GPU**：DirectML provider 支持 OK；无 GPU 时自动回落 CPU。AI_POOL_SIZE=1（Mutex 串行）——实测池化无收益（AI 非瓶颈且每 session 线程减半变慢）。**M9 实验性 `--gpu`（`--features gpu` 构建）实测：119 张 16.7s vs CPU 17.4s（~4%）——单张小批量 DML 吃不满 GPU，无显著收益，保持实验性、默认关闭**。
- **权重校准**：默认权重 0.30/0.25/0.15/0.15/0.15（和=1.0，M5 决策 A：曝光 0.25 压欠曝虚高）；多场景用 `--config` 按需调整，内置 `presets/` 5 份（portrait/stage/highkey/sports/lowlight）。
- **Lightroom 读 XMP 星级**：命名已改为 LR 约定（`<stem>.xmp`），但**仍缺一次真实导入验证**（用户在 LR 里导入目录，确认星级/子分显示）。Phase 1 唯一没被实测过的交付物。
- **配对键（M7 已修）**：此前所有索引/配对/侧车去重都只用"文件名主干"，而索尼编号在 DSC09999 后回绕 → 上万张跨目录必然出现同名文件，会让分数/连拍/侧车互相覆盖。现改为「目录 + 主干」配对键。
- **遮挡脸/超大脸漏检**（未解决）：口罩/头盔遮挡时 SCRFD 置信度会掉到 0.2 附近，超大人脸特写也会漏检（PORTRAIT_TEST 0.117）。当前对评分的影响已通过"主体感知单向修正 + 构图只计主体脸"降级处理，未换模型。

## 5. 已停止的 darktable RAW 开发研究（原设计 §9）

> 下方保留历史技术参数、命令陷阱、镜头普查和实验数据，便于复查。其“已确认”“待实施”“后续”均是停止前的措辞；本方案于 2026-09-18 停止，所有任务均不在当前计划。文中 §9.x 为原研究编号；`p2_notes/` 等本机研究路径并非发行依赖。

> **⚠️ 状态更新（2026-09-18）：darktable 批量开发管线已停。**
> 方向由用户拍板：firstcut 回归**筛选工具本位**——新增"建议曝光 EV → XMP"
> （`firstcut:suggestedEV` 信息字段 + `crs:Exposure2` Lightroom 开发字段，
> 带外才给建议、±2 EV 钳位、1/3 档取整；配置 `exposure_suggest_cap`），
> 星级/筛选名单本就在 XMP 里，**实际开发仍由 Lightroom 完成**。
> 本章 §9.1~§9.4d 的调研结论（darktable-cli / 侧车模板 / 16bit TIFF / EV patch）
> **全部留档备查、不再实施**。设计文档由用户重写，本章仅作历史档案。

> 需求：选片后不想进 LR，希望自动完成"自动曝光/色调 + 镜头校正 + AI 降噪"批量出图。
> 结论（已调研）：**可行**，采用"Rust 编排 + darktable-cli 引擎"分工。

### 9.1 开源方案调研结论

| 需求 | 方案 | 状态 |
|---|---|---|
| 批量 RAW 开发引擎 | [darktable-cli](https://darktable-org.github.io/dtdocs/en/special-topics/program-invocation/darktable-cli/)（无头批处理，Windows 有官方构建，索尼 ARW 支持好） | ✅ 成熟 |
| 镜头校正 | [lensfun](https://github.com/lensfun/lensfun)（darktable 内置，开源镜头数据库）；冷门头可自校准（[lens_calibrate](https://gitlab.com/cryptomilk/lens_calibrate)） | ✅ 成熟，覆盖视镜头而定 |
| AI 降噪 | ~~darktable neural restore~~ **实测不可用于 CLI**（见下）→ 改用 `rawdenoise`（RAW 域小波）+ `denoise (profiled)`（主） | ❌ neural restore 仅 GUI；替代方案 ✅ 已实测 |
| 自动曝光/色调 | darktable exposure 模块 auto-exposure + Lua（[autostyle](https://darktable-org.github.io/luadocs/lua.scripts.manual/scripts/contrib/autostyle/)）；**Phase 1 的曝光分析直接产出每张补偿值写入 XMP** | 🟡 可达成，8 成效果 |
| 备选（不推荐） | 纯 Rust 自研：rawler 解码 + 自写色调映射 + `ort` 跑 NAFNet/SCUNet ONNX（[NAFNet](https://github.com/megvii-research/NAFNet)、[ONNX 权重](https://huggingface.co/qualcomm/NAFNet-DeNoise)） | 色彩科学差距大，工作量巨大 |

### 9.2 Phase 2 架构（已确认）

```
[Phase 1 评分工具] → 保留照片清单 + 每张曝光补偿建议
        ↓
[Phase 2 Rust 编排器]
  1. 为每张 ARW 生成 XMP 侧车（darktable 可读）：
     - lens correction: auto（lensfun）
     - exposure: 补偿值（来自 Phase 1 分析）
     - denoise: `rawdenoise` + `denoise (profiled)`（按 ISO 调 `strength`；
       **不是** neural restore——它没有 CLI 入口，见 §9.3）
     - 白平衡: 按场景（日光/阴天/自动）简单预设
  2. 循环调用 darktable-cli 批量导出：
     - 16bit TIFF（归档，保留后期空间）
     - 高质量 JPG（分享/预览）
  3. 输出到 developed/ 目录，不动原片
```

### 9.3 Phase 2 风险

- **lensfun 对用户镜头的覆盖**：需实测；缺失时用 lens_calibrate 自校准（一次性的活）。
- **neural restore 无法用于 CLI 批量（已实测定案，2026-09-09）**：
  模块**存在**且 **RAW 域降噪（RawNIND / PR #20854）确已合入 5.6**，但它是 `dt_lib_module_t`
  （GUI 工具面板）**而非 iop**：不接收 XMP 参数、不在 pixelpipe 里、靠人点 Process 触发，
  自己跑后台任务写新 DNG/TIFF 再重新导入，源码里没有 `dt_dev_add_history_item`。
  `darktable-cli` 侧证据：① `--help` 无任何 AI/模型选项；② `--luacmd` 脚本**从不执行**
  （Lua 走 `dt_lua_async_call()` 异步投递，而 CLI 是同步直线流程从不进主循环，
  `init_gui=FALSE` 还导致 `dt_lib_init()` 不跑、模块连注册都没发生）；
  ③ 二进制里确有 `darktable.ai` Lua API，但只有 GUI 版接受 `--luacmd`。
  **点一次 GUI 也救不回来**（不是一次性下载问题）。
  → **替代方案（已实测）**：`rawdenoise`（RAW 域小波，轻）+ `denoise (profiled)`（主）。
  两者都是标准 iop、可走 XMP。**`denoise (profiled)` 内置相机噪声 profile 并按 ISO
  自动插值**，"按 ISO 分级降噪"是它的原生行为，`strength` 即分级主旋钮。
  modversion / `op_params` 二进制布局见 `p2_notes/neural_restore.md`；
  验证方式：同源同参数只改 `<darktable:enabled>` 1→0，两个 16bit TIFF 的 md5 不同。
  保留未来路径：若上游给 neural restore 加 CLI 入口再重评。
- **darktable 读 XMP 侧车的字段**：需用真实照片验证一遍（M4 阶段已计划验证 XMP 星级，可一并做）。
- **GPU 需求**：neural restore 走 ONNX Runtime，无 GPU 会慢，需确认机器配置。

### 9.4 镜头清单与校正策略（**已按实机实测重写**，2026-09-09）

> 原表基于"听说有哪几支镜头"填写，与实际库存严重不符：实测（`pic_process scan F:\PS_Process`，
> 全库 15979 文件 / 8603 ARW）发现占比最高的 FE 24mm GM、E 18-135、Viltrox 23 都没被列出，
> 而 200-600G 是**已购入但尚未拍摄**（保留，不删）。
> lensfun 结论由解析本机全部 56 个 XML / 1563 条 `<lens>` 条目 + 上游 master 交叉验证得出。

**实际镜头与 lensfun 覆盖实测**：

| 镜头 | ARW 张数 | Sony E 口 lensfun 条目 | 支持校正项 | 结论 |
|---|---|---|---|---|
| FE 24mm F1.4 GM | 2569 | ✅ `mil-sony.xml` | 畸变+色差+暗角 | 直接用 lensfun |
| E 18-135mm F3.5-5.6 OSS | 1731 | ✅ `mil-sony.xml` | 畸变+色差+暗角 | 直接用 lensfun |
| **Sigma 50mm F1.4 DG DN \| Art 023** | **1518** | ❌ **无条目** | — | **🔴 必须兜底（占 17.6%，最高优先级）** |
| Viltrox 23mm F1.4 E | 1397 | ✅ `misc.xml` | 畸变+色差+暗角 | 可用；**标定源自富士 X-T20**（cropfactor 1.53 vs E 口 1.534），失真/TCA 可靠、**暗角可能略偏**，建议抽样目视 |
| E 70-350mm F4.5-6.3 G OSS | 1231 | ✅ `mil-sony.xml` | 畸变+色差+暗角 | 直接用 lensfun |
| FE 24-70mm F2.8 GM II | 151 | ✅ `mil-sony.xml` | 畸变+色差；**暗角缺** | 用 lensfun，暗角另想办法（**一代 GM 的暗角系数不可套用二代**） |
| Sigma 70-200mm F2.8 DG DN OS \| Sports 023 | 4 | ❌ 无条目 | — | 需兜底（优先级极低） |
| FE 200-600mm F5.6-6.3 G OSS | 0（**已购入未拍摄**） | ✅ `mil-sony.xml` | 畸变+色差+暗角 | 已确认覆盖，拍到即可用 |

**关键背景**：
- darktable 的 lens correction 模块只走 **lensfun 数据库**，按 EXIF 自动匹配，匹配不到则"无 profile"（[官方文档](https://darktable-org.github.io/dtdocs/en/module-reference/processing-modules/lens-correction/)、[社区反馈](https://github.com/darktable-org/darktable/issues/11022)）；内嵌校正数据（embedded DNG corrections）仅 DNG 支持（[PR #12880](https://github.com/darktable-org/darktable/pull/12880)），**ARW 不适用**。
- 索尼 ARW 的 maker notes 里**内嵌镜头校正数据**（畸变/暗角/色差），RawTherapee 的 Lens/Geometry 可读取（[参考](https://photo.stackexchange.com/questions/114615/raw-therapee-lens-geometry-correction-sony-a6100/114621)）→ 可作**备用引擎**。
- 本机 lensfun 版本 **0.3.4**（随 darktable 5.6.1）。
- **对 Phase 1 无影响**：索尼 JPG 出厂即烘焙镜头校正，评分用 JPG 天然已校正。
- `18-135mm` 与 `23mm f/1.4` 在 `mil-fujifilm.xml` 里也有同名条目（`XF18-135mmF3.5-5.6R LM OIS WR`、`XF23mmF1.4 R`），**那是富士 X 口，不是命中**；两支镜头各自另有真正的 Sony E 口条目。
- **不要假定"命中 Sony E 口 = 三项校正齐全"**：`FE 24-70 GM II` 缺暗角；`FE 70-200mm f/2.8 GM OSS` 只有畸变。通用判定逻辑必须**逐条目解析 `<calibration>`**，不能只看是否命中。

**兜底链路（按优先级）**：darktable lensfun 命中 → RawTherapee 读索尼内嵌数据（同一张 ARW 换引擎出图）→ lens_calibrate 自校准 → 跳过（仅限畸变可忽略的镜头）。

> ⚠️ **`lensfun-update-data` 类的升级路径救不了**：已抓上游 master 验证，
> **两支适马（50/1.4 DG DN、70-200 DG DN OS）上游同样没有条目**，只能自建 profile 或跳过。
> 真正"完全无 lensfun"的合计 **1522 张 / 8603 = 17.7%**。
> 建议把"两支适马是否进入上游"做成**周期性检查项**。

### 9.4b P2-M1 侧车方案（已实测定案，2026-09-09）

> 结论来源：对 darktable 5.6.1 逐字段/逐偏移实测 + 源码核对（详见 `p2_notes/xmp_fields.md`，
> 成品侧车 `p2_notes/reference/DSC00886.ARW.xmp`，机器可读模板 `xmp_template_ILCE-7M5_dt5.6.1.json`）。

**① 侧车命名：两条命名各司其职，不冲突**

| 文件名 | 谁读 | 读什么 |
|---|---|---|
| **`<原文件名含扩展名>.xmp`**（`DSC00886.ARW.xmp`） | **darktable 正牌侧车** | 全部，**含 `darktable:history`** |
| `<stem>.xmp`（`DSC00886.xmp`） | darktable 仅经 `dt_lightroom_import()` | **只有 rating / Label / 关键词；`darktable:history` 被静默忽略** |
| `<stem>.xmp` | **Lightroom / ACR（M7 决策）** | rating 等 |

→ **Phase 1 保持现状不动**（写 `<stem>.xmp` 给 Lightroom）。
**Phase 2 编排器另写 `<原名>.ARW.xmp`**（含完整 history + 同时带上 `xmp:Rating` / `firstcut:*`，
因为正牌侧车存在时 LR 风格那份不再被合并）。两者共存，darktable 优先读正牌侧车。
`darktable-cli` 会**自动发现**输入同目录的同名侧车，且 **CLI 全程不写侧车**（只读不写，安全）。

**② 四个"写错就静默失效"的点**

- 顶层 `darktable:xmp_version` **合法范围只有 2..5**（缺失/0/1 → legacy 解析 → history 静默丢弃；≥6 → 不支持）。
- 每条 history 必须齐 `operation` + `params` + `modversion`，缺任一 → **整条丢弃**。
- `darktable:enabled` 默认是 **0（关闭）**，必须显式写 1。
- `darktable:history_end` = 实际应用条数。

**③ 落地方式：完整栈模板 + 定点补丁（不能用"部分侧车"）**

部分侧车实测不可用：只写 1 条 exposure 时 darktable 只补 9 条强制模块（**不补 sigmoid / color
calibration**），结果 512px mean 128.1（无侧车默认 138.7）并随参数剧烈漂移（EV=2.2 时过曝 65.6%）。
→ 做法：用 `darktable-cli <copy.ARW> <out> --library <tmp>/library.db --core --configdir <tmp>/cfg`
导入一张该机型 ARW，从临时库 `history` 表抓**默认栈**（本机 ILCE-7M5 = 11 条、ILCE-6700 = 13 条，
含 modversion 与原始 hex；6700 的 lens/denoiseprofile 自带，见 §9.4d ⑦ 重抓流程），
之后只做三件事：**原地** patch exposure 的 EV、**原地替换** temperature 的系数、**patch-or-append**
lens 与 denoiseprofile（栈里有则原地、无则追加，§9.4d ②）。
实测完整栈原样回灌与"无侧车导出"逐像素几乎一致（Δmean 0.002/255）→ 模板法可靠。

**④ 四个模块的 `op_params` 结构（小端 hex，均无指针/变长，可安全手写）**

| 模块 | modversion / 大小 | 关键字段 |
|---|---|---|
| `lens` | v10 / 356B | 偏移 332 `has_been_set=0` → 走 EXIF+lensfun **自动匹配**（camera[]/lens[] 留空）。默认栈里没有，需追加 |
| `exposure` | v7 / 28B | 偏移 8 = EV。**陷阱**：本机 ARW 机身曝光补偿 −1.3EV，默认 `comp_bias=1` 会补上 → 我们的 ΔEV 要**原地读模板自带 EV 再加 Δ**（7M5 基线 0.7 / **6700 基线 1.2**，§9.4d ②），且**保持 comp_bias=1**。**不得硬编码 0.7** |
| `denoiseprofile` | v12 / 416B | `a[0]` 必须 **−1.0f** = 按相机+ISO 自动取 profile（**写 0 实测导出几乎全黑**）；偏移 8 = `strength`；mode=4 为 wavelets-auto |
| `temperature` | v4 / 20B | **没有 Kelvin/tint**，只有红绿蓝系数；`preset=2` 才用给定系数（`preset=0` as-shot 会被 EXIF 覆盖）；**必须原地替换**默认栈那条（追加会变两个实例叠加） |

**⑤ 版本强绑定（相机绑定已实测弱化）→ 必须做防御性校验**

模板与 **darktable 版本**强绑定（modversion/params 长度随版本变），但 2026-09-18 实测
7M5 与 6700 默认栈共有模块**逐字节相同**，机型相关值只有 exposure 的 EV 基线（§9.4d ②）
→ 按机身+版本存模板保留（防升级），不必再为颜色矩阵担心跨机型。Rust 侧用
`darktable-cli --version` 做模板键，并且**导出后必须校验非全黑**——版本或 params 长度写错时
darktable **不报错**，直接给黑图或未初始化内存（`modversion` 偏新 → params 是未初始化内存）。

**⑥ 仍待确认**：`firstcut:*` 在 GUI「改图→保存侧车」后是否 100% 保留（源码上会保留）；
自定义 `iop_order_version`；Kelvin/tint → 相机 RGB 系数的精确换算（`wb_presets.json` 无 ILCE-7M5）；
降噪强度分级最优值需目视。~~16bit TIFF 导出参数~~ → 已解决（§9.4d ③：
`--core --conf plugins/imageio/format/tiff/bpp=16`）；另新增 LR 对 darktable 16bit TIFF 的兼容性待测（§9.4d ④）。

### 9.4c P2-M2 编排器方案（已实测定案，2026-09-09）

> 结论来源：darktable-cli 5.6.1 Windows 下 27 组用例 + 并发压测 + 基准（详见 `p2_notes/cli_batch.md`）。

**① 并发模型：一张一进程 + worker 池，默认并发 6**

实测（12 张全分辨率 16bit TIFF）：P=4 → 27.4s，**P=6 → 24.0s（最优，2.0s/张）**，
P=8 → 26.2s（退化），**P=12 → 81.2s（比串行还慢 2×，灾难）**。
原因：单进程已能吃满 16 逻辑核，超订导致线程爆炸 + 换页（P=12 时单张 CPU 从 12s 涨到 48s，约 200 线程）。
→ `并发 = clamp(cores/3, 2, 8)`，并加**内存闸门**（全分辨率每张预留 1.2GB，2048px JPG 预留 0.6GB）。
**不要用"一进程多图"**：位置参数传多个输入只有第一张被导出（其余静默忽略、exit 0）；
只有重复 `--import` 才真多图，但牺牲逐图错误归因、内存线性增长且不更快。

**② 成功判定：必须以文件系统为准，绝不能靠退出码**

实测 3 张里 2 张成功 + 1 张损坏 → **退出码仍是 1**（整批聚合，无法区分全败/部分败）。

```
运行前：create_dir_all(outdir)；记录 outdir 已有文件
spawn ：darktable-cli <输入正斜杠绝对路径> <输出目录正斜杠>/ --out-ext <ext> --apply-custom-presets 0
判定  ：exists(outdir/(stem+"."+ext)) && size > MIN_BYTES   # jpg ≥ 10KB，tif ≥ 1MB
```
日志兜底**只能匹配英文模块名前缀**（`[export_job] exported to`、`[imageio_storage_disk] could not export`、
`[rawspeed] ... corrupt`）——**本机运行期错误提示是中文的**（随 locale 变），匹配中文必失效。

**③ 三个必须做的前置动作（否则静默产出垃圾）**

1. **路径全部 `\` → `/` 并在构造参数后断言不含 `\`**：反斜杠被**直接删除**而非转义，
   `F:\p2cli\out\` 会落盘到 `F:\p2cli\p2cliout\`，exit 0 且无警告。
2. **自己先 `create_dir_all` 输出目录**：目录不存在时 `$(FILE_NAME)` 不展开，
   写出字面名文件 `$(FILE_NAME).jpg` 且 **exit 0、日志照样写 exported**。
3. **加 `--apply-custom-presets 0`**：GUI 在跑时 CLI 默认 0.66s 内 `database lock ... aborting` exit 1；
   该选项是官方给的"multiple instances"开关。注意 `--library` **不能**绕过锁。

**④ 另外 7 个坑**

- 加了 `--apply-custom-presets 0` 后 **`--style` 必然失败**（style 存数据库里）→ **渲染方案必须走预生成
  含 history 的 XMP，不能用 `--style`**。
- **重跑不覆盖**：同目录已有产物时新文件叫 `_01`/`_02`，导致"按预期文件名判成功"的编排器第二次跑会误判全失败。
  → 每 worker 独立输出目录，或跑前清空，并按"本次新增文件"判定。
- 位置参数传第二张图被静默忽略（exit 0）；`--import DIR` **不递归**。
- **配置目录无法隔离**（`libdarktable.dll` 里没有 `DARKTABLE_CONFIGDIR`/`XDG_CONFIG_HOME`，"假 LOCALAPPDATA"无效）；
  CLI 每次运行都会重写 `darktablerc`（幂等，未观测到损坏）→ 当**共享资源**处理。
- **中文路径其实可用**：之前的"不可用"是 PowerShell 5.1 把 UTF-8 无 BOM 的 `.ps1` 按 GBK 解码造成的假象；
  Rust 用宽字符 API 无此问题。但反斜杠的 `--library` 路径会触发 `GLib-CRITICAL g_utf8_to_utf16` 断言。
- **不要用 `--library`**（它的语义是"从库读 history **而非** XMP 侧车"，指向没导入过这些照片的库 = 全部按默认导出）。
- `--verbose` 无帮助；无 `--bpp`（help 标 unsupported）、无 `--format`。

**⑤ 性能与资源（写 Rust 用得上）**

| 输出 | 单张墙钟 | 峰值工作集 | 产物 |
|---|---|---|---|
| 全分辨率 16bit TIFF | **~8.1s** | 870MB~1.1GB | 45.5MB |
| 全分辨率 JPG | ~5.0s | ~1.05GB | 8.77MB |
| 2048px JPG | ~3.7s | ~0.36GB | 736KB |

内存是阶梯式：~70MB 解码 → ~360MB → **865MB 平台**。TIFF 已验证：4688×7032、16bit、3 通道、
Compression=8 (Adobe Deflate)、内嵌 ICC 9012B。**产物体积随机身/内容差异大**：6700 全幅样片实测
117–127MB（§9.4d ③）→ 磁盘预算按 **130MB/张**（8000 张 ≈ 1TB），45.5MB 只是下限参考。

**⑥ 安全结论（重要）**：CLI **不写也不改 XMP 侧车**（前后 MD5 一致），源图无侧车也不新建
→ **`F:\PS_Process` 原库可以安全只读使用**。

**⑦ 建议命令模板（2026-09-18 更新，含 16bit 强制，见 §9.4d ③）**

```
darktable-cli.exe <正斜杠输入绝对路径> <已创建的正斜杠输出目录>/ --out-ext tif --apply-custom-presets 0 ^
  --core --conf plugins/imageio/format/tiff/bpp=16
# 勿用 --style / --library / --bpp / --verbose（--bpp 被 help 标 unsupported；
# 位深只能经 --core --conf 覆盖，profile 默认 bpp=8 会静默出 8bit 图）
# JPG 输出同理可带该参数（对 jpg 无效但无害），用于 P2-M2 验收交叉核对（§9.4d ⑤）
```

**⑧ 待确认**：`--apply-custom-presets 0` 在"用户已配置自定义预设"时是否影响成像
（本机无自定义预设，无法实测差异；P2 用预生成 XMP 正好规避该依赖）；OpenCL 是否真启用
（`cltest` 跑 10 分钟未返回，本轮 CPU 时间占比像纯 CPU）。

### 9.4d 动作 A 真图冒烟实录（2026-09-18，样片来自 ILCE-6700 机身，修订 §9.4b/§9.4c）

> 执行人：DSH（qwen/qwen3.8 与用户结对）。样片从 `F:\PS_Process` **只读**挑出并拷入
> `p2_notes/_smoke/input/`（全库 15,979 文件重扫 `_main_scan.csv` 复核镜头普查，与 §9.4 完全一致）。
> 样片：**DSC02906**（20240324_机场，E 70-350 G，lensfun 命中路径）+ **DSC08051**
> （20241006_家里随拍，Sigma 50/1.4 Art，lensfun 无条目兜底路径）。
> **机身普查**：ILCE-6700 = 8360 ARW（97%）、ILCE-7M5 = 239、ILCE-7RM5 = 4 → 模板必须按机身备，6700 是主力。

**① EV 补丁生效（隔离实验，同图同模板只变 delta）**

| delta-ev | 写入 EV（6700 基线 1.2） | JPG mean/255 |
|---|---|---|
| 0 | 1.2 | 131.0 |
| +0.5 | 1.7 | 154.6 |
| +3.0 | 4.2 | 233.1（近裁剪，符合 3 档预期）|

同 EV 下 16bit TIFF 与 JPG 像素对拍 5 点差 ≤2/255 → 侧车被读、EV 浮点补丁落地、管线非全黑。

**② 模板发现（修订 §9.4b ③/④/⑤）**：7M5 与 6700 的默认栈**共有 11 个模块逐字节相同**
（rawprepare/demosaic/colorin/colorout/temperature/gamma/highlights/channelmixerrgb/flip/sigmoid +
exposure 的 black/percentile/comp 字段）。唯一机型相关值：**exposure 默认 EV 基线 7M5=0.7、6700=1.2**。
且 6700 默认栈**自带** lens(v10, 356B) + denoiseprofile(v12, 416B)（共 13 条），7M5 不带（11 条）——
6700 自带的两个 blob 与我们设计的 auto blob（`has_been_set=0` / `a[0]=-1.0f`）**逐字节一致**。
→ 生成器逻辑定案为 **patch-or-append**（栈里有则原地补丁，无则追加，`history_end`=最终条数）；
→ EV 补丁定案为"**原地读模板 EV 加 delta**"，**不得硬编码 0.7**（那是 7M5 的值）。
→ 按机身+版本存模板的做法保留（防 darktable 升级），但实测机型相关面远小于预期。

**③ 16bit TIFF 解锁（修订 §9.4b ⑥ 与 §9.4c ⑤/⑦）**：profile 默认
`plugins/imageio/format/tiff/bpp=8`（此前导出全 8bit 的根因）；`--bpp` 被 help 标 unsupported。
强制 16bit 的正式姿势（P2-M2 命令模板必须带）：

```
--core --conf plugins/imageio/format/tiff/bpp=16
```

实测 6700 全分辨率（6520×4176 = 27MP 全幅，**非降采样**，`--width/--height` 默认 0）16bit TIFF
= **117–127MB/张**（§9.4c ⑤ 的 45.5MB 是另一机身的样片，压缩比随画面内容变化很大）
→ **磁盘预算按 130MB/张**，8000 张 ≈ 1TB。

**④ TIFF 压缩怪癖（进坑清单，待用户周末 LR 实测）**：darktable 5.6.1 写的 TIFF `Compression`
tag = 8（TIFF 规范名义 = LZW），但 strip 流实际是 **zlib**（`78 9c` 魔数，4176 条 strip 逐条
精确解压尺寸吻合）。GDI+/.NET 读取正常且像素正确；**严格 LZW 阅读器（Lightroom 是否算）未知**
→ 验收项：周末 LR 导入 `p2_notes/_smoke/deliv/` 的 16bit TIFF。若打不开，杠杆是 profile 的
`plugins/imageio/format/tiff/compress`（当前 =2，可试 0=不压缩/其他档）。

**⑤ P2-M2 验收策略（修订 §9.4c ②）**：本轮自写 Python TIFF 解码器在 strip→行映射上有 bug
（8bit 文件解出二值数据、16bit 文件中心像素与 GDI+ 不符，原因未定位，**不要再信任自解 TIFF
像素**）。定案：每张图**另导一份同 EV 的 JPG**（darktable 自己当解码器），用 JPG 做
mean/峰值/过曝率交叉核对；TIFF 本体只验"存在 + 大小 + BPS tag=16"。

**⑥ 暗角实测**：70-350（lensfun 路径）JPG 3×3 网格 corner/center=**0.947**、
edge/center=**0.968**（未校正约 0.7）→ 镜头校正生效；Sigma 50（兜底路径）导出干净无畸变，
该场景光照不均（人物阴影）径向指标不适用，符合 §9.4 预期。

**⑦ 默认栈重抓流程（新机身/darktable 升级时用）**：新目录放空 `library.db`（0 字节）→
`darktable-cli <图> <out>/ --out-ext jpg --apply-custom-presets 0 --library <dir>/library.db`
→ 读该 db 的
`select num,module,operation,op_params,enabled,blendop_params,blendop_version,multi_priority,multi_name,multi_name_hand_edited from history where imgid=1 order by num`
（6700 抓得 13 条，数据全在 library.db 本体，无独立 data.db）。

**⑧ 产物与资产**（`p2_notes/_smoke/`，随 p2_notes 不入库）：`deliv/` 4 件待用户肉眼验收
（DSC02906/DSC08051 各 16bit TIFF + 同 EV JPG，EV=1.7、降噪开）；`make_sidecar2.py`（v2
机身感知生成器，**P2-M1 Rust 移植蓝本**：模板 JSON + 版本键 + patch-or-append + 双命名）、
`stack6700.json`、`tiff_probe.py`/`probe16.py`/`cross_cmp.py`/`check_sidecar.py`（验收工具）、
`lib6700/`（抓栈用临时库）。`reference/xmp_template_ILCE-6700_dt5.6.1.json` 已就位。

**⑨ 仍未决**（沿用 §9.4b ⑥/⑧ + 本轮新增）：`firstcut:*` GUI 保存后保留性；OpenCL；
Kelvin→RGB 换算；降噪强度分级目视；**LR 对本轮 16bit TIFF 的兼容性**。

### 9.5 历史里程碑（已停止，不实施）

- **P2-M1**：环境验证 —— 安装 darktable，darktable-cli 手动跑通一张 ARW 全流程（校正+降噪+导出 TIFF/JPG）
- **P2-M2**：Rust 编排器 —— 生成 XMP 侧车 + 调用 darktable-cli + 并发批处理 + 进度/日志
- **P2-M3**：联动 Phase 1 —— 曝光补偿值写入侧车、按评分阈值决定开发名单
- **P2-M4**：实测调优 —— 你的镜头跑一轮，调 lensfun 覆盖、降噪强度分级、输出验证

### 9.6 历史实施清单（已停止，不实施）

> 以下是已停止的 darktable 路线原始清单，仅供追溯；未勾选项不代表当前产品待办。

**P2-M0 环境与可行性（历史记录；后续结论汇总于 §9.3–§9.4d）**
- [x] 装 darktable 5.x（Windows 官方构建），确认版本 ≥ 5.0（实装 5.6.1）
- [x] `darktable-cli` 命令行跑通一张 ARW → TIFF/JPG（正斜杠路径、`--out-ext` 等用法纠错见 §9.4c）
- [x] `darktable-cltest` 确认 OpenCL/GPU 状态（10 分钟未返回 → 按纯 CPU 对待；neural restore CLI 批量不可用）
- [x] 确认 neural restore 是否有 RAW 域降噪（实测不可用：GUI 面板非 iop，CLI 不执行 → 降噪改 rawdenoise + denoise profiled）
- [x] lensfun 对 4 支镜头（24-70 GM II / Sigma 50 DG DN / 70-350G / 200-600G）的覆盖实测（已按实测重写 §9.4，并扩展为 8 支镜头全库普查）

**P2-M1 侧车模板与字段映射——方案已实测定案（2026-09-18 冒烟，见 §9.4d），Rust 未开工**
- [x] 确定写哪些模块：lens(auto) / exposure / denoiseprofile；白平衡暂缓（preset=2 路径已调研，Kelvin 换算未决）
- [x] 手工造一份侧车，验证 darktable 确实读取并生效（EV 阶梯隔离实验 + 同 EV TIFF/JPG 像素对拍，§9.4d ①）
- [x] Phase 1 的 EV 偏移 → darktable exposure 模块参数换算（**原地读模板 EV 加 delta**，7M5=0.7/6700=1.2，§9.4d ②）
- [ ] 降噪强度分级策略（按 ISO？按 Phase 1 噪点分？）——需目视
- [ ] `src/sidecar.rs` Rust 移植（蓝本 `p2_notes/_smoke/make_sidecar2.py`）+ 四模块 blob 字节级单测 + 模板版本键校验

**P2-M2 Rust 编排器**
- [ ] 新子命令 `develop <目录>`：读 Phase 1 CSV → 写 XMP → 调 `darktable-cli` → 输出 `developed/`
- [ ] 并发批处理（darktable-cli 是独立进程，可并行 N 个）+ 进度日志
- [ ] `--min-stars` 只开发达标的照片；断点续跑（已有输出跳过）
- [ ] 失败重试与错误汇总；不动原片
- [ ] 输出格式：16bit TIFF（归档）+ 高质量 JPG（分享），路径/命名规则

**P2-M3 联动与验收**
- [ ] 曝光补偿回写 Phase 1 侧车（LR 里也能看到建议值）
- [ ] 开发结果与原片/ Lightroom 出图对比（主观 + 直方图）
- [ ] 逐支镜头验证校正效果

**P2-M4 性能与规模**
- [ ] 100 张 ARW 批处理耗时基线
- [ ] 磁盘占用（16bit TIFF 体积）；上万张的排队/中断恢复

**需要你拍板的开放项**
- [x] 输出只要 JPG，还是 TIFF 归档 + JPG 都要？→ **16bit TIFF 归档 + JPG**（Phase 2 原始需求；TIFF 兼容性待 LR 周末实测，§9.4d ④）
- [ ] 要不要保留 darktable 的 `.xmp` 调色参数（方便以后手改）？注意这会让目录多一批文件
- [ ] 白平衡：完全自动 / 按场景预设 / 保留机内？
- [ ] 降噪强度按 ISO 分级，还是按 Phase 1 的噪点分分级？
- [ ] 是否额外导出"高分片名单"（CSV/文本）给 LR 用？

## 6. 评分、配对和缓存决策表（原设计 §10）

> 原则：**默认值必须对任何场景站得住脚，不能是对某一批样片的拟合**；
> 场景差异交给 `--config` / `presets/`，而不是塞进默认曲线。

| # | 问题 | 决策 | 依据 |
|---|---|---|---|
| M6-1 | 曝光用"偏离中灰多少码值"判定，舞台黑幕布/白裙白背景被误判 | 改为 **EV 容差带**（±1 档满分，-4/+2 档归零，两侧独立） | ±1 EV 由 sRGB 传输函数算出恰好是码值 92~176，是 AE 的正常波动范围；容差带只惩罚真正越界的曝光 |
| M6-2 | 主体脸亮度参与曝光会不会过度依赖人脸检测 | 只做**单向修正**（夹在"全图 ~ 中灰"之间） | 暗色/亮色误检框无法压低或反向推高分数；误检代价可控 |
| M6-3 | 场景差异怎么落地 | 内置 **5 份场景预设**（`config-template --preset`），预设只调权重与 EV 容差 | 回应"不同场景要搞配置"的要求，避免把场景特例写进默认值 |
| M6-4 | 换 `--config` 后结果不变 | 缓存键加入**配置指纹** | 此前是静默 bug：缓存命中直接返回旧的五维分数，用户会以为配置没生效 |
| M6-5 | 如何验证曲线不是"拟合样片" | 单元测试断言 **sRGB↔EV 换算、容差带内满分、带外单调、两侧独立、单向修正** | 测试输入是合成码值而非真实照片，结论与样本集无关 |
| M7-1 | 侧车命名 LR 读不到（实际是 darktable 约定） | 改为 `<stem>.xmp` | 该格式 LR/ACR 与 darktable **都能读**；一份侧车两边通用（用户确认最终用 LR） |
| M7-2 | 绝对星级阈值失去区分度（119 张全落 4~5 星） | 默认改为**批次内相对分档**（10/30/65/90 百分位），绝对模式保留 | 各维度为防误杀都带中性地板，绝对分数下限被抬高；用户确认"批次内保持标准即可"。总分仍写 CSV，跨批次比较看分数 |
| M7-3 | 同名文件跨目录互相覆盖 | 配对/索引键改为「目录 + 主干」 | 索尼编号 9999 回绕，上万张场景必然重名；实测两目录同名文件分数现已独立 |
| M7-4 | 连拍排序用默认权重而非 `--config` 权重 | 传入实际权重 | 配置与结果不一致属静默 bug |
| M8-1 | M7-3 的「目录+主干」配对键把"JPG/RAW 分放两个子目录"也排除了（**回归**，集成测试失败） | 改为两步配对：同目录优先 + 无歧义跨目录兜底；有歧义不配对 | 两种场景必须区分：回绕是"不同照片同名"，分目录是"同一张照片分放"；实测 testpic 31 文件 30 配对 |
| M8-2 | `--config` 失败只警告后用默认值跑完 | 改为硬报错退出；未知字段拒绝；`star_mode`/百分位校验 | 与 M6-4 同类：显式传参却静默失效后果更重 |
| M8-3 | EXIF Orientation 5/7 变换互换 | 改为 `flip_horizontal(rotate90(img))` / `flip_horizontal(rotate270(img))` | 5=转置、7=反对角线；原写法得到的是 rot180/transpose 互换（CACHE_VERSION 10→12） |
| M8-4 | pose 兜底只在 `faces == 0` 时触发，只有小脸时被跳过 | 条件改为 `sharpness_region.is_none()` | 与"SCRFD 漏检 → pose"的设计意图对齐 |
| M8-5 | 配置指纹含权重/星级阈值，改权重触发全量重算 | 指纹只保留曲线参数 | 改权重是最高频调参动作，却不需要重算 |
| M8-6 | pose 输出是否需 sigmoid 两处注释矛盾 | 实测 248 个置信度全在 (0,1) → **已 sigmoid**，只统一注释 | 换模型前需重新核验值域 |
| M9-1 | 每个姿态簇保留几张 | **3 张**（`keep_k` 默认 2→3，CLI `-k` 同步） | 用户定；表情成功率低的连拍留足备选 |
| M9-2 | 单组保留总量失控（90 帧连拍多姿势） | `burst_group_cap = 20`，超出按总分从高到低截断 | 用户定；截断只动 keep 标记，不动排名 |
| M9-3 | 默认启用还是加开关 | **默认启用** + `dedup.adaptive_keep = false` 回退旧行为（逐字节一致） | 对日常使用零操作成本；旧行为可复现 |
| M9-4 | 姿态聚类阈值怎么定 | 0.25（描述子欧氏距离），`pose_cluster_threshold` 可配 | 棚拍实测双峰（同姿势 0.02~0.06 / 跨姿势 0.46~0.51），0.25 居中余量 4×；舞台连续场景粒度合理 |
| M9-5 | M9 会不会改变评分 | 不会：只改 burst 三列；archive 119 张回归星级逐档一致（12/24/41/31/11） | 描述子仅进去重，不参与五维分数 |

## 7. 研究参考（原设计 §1）

| 项目 | 参考点 |
|---|---|
| [pixcull](https://github.com/ChrisChen667788/pixcull) | 6 轴评分维度划分、XMP/IPTC 导出、近重复分组 —— 维度设计与输出格式直接参考 |
| [facet](https://github.com/ncoevoet/facet) | "传统指标 + AI 美学"混合打分架构 |
| [best-photo-picker](https://github.com/Arkalogy/best-photo-picker) | 质量评分 + 感知去重组合 |
| [RAWviewer](https://github.com/markyip/RAWviewer) | RAW 选片 + 星级 + XMP 的本地工作流 |
| [digiKam Image Quality Sorter](https://docs.digikam.org/es/_sources/maintenance_tools/maintenance_quality.rst.txt) | 纯算法基线：清晰度/噪点/曝光的经典实现 |
| [NIMA](https://github.com/bencoster/Neural-IMage-Assessment) | AI 美学模型（PyTorch 权重，需转 ONNX） |

### 7.1 UI 选型理由

原 gallery 使用内嵌 base64 的单文件 HTML，适合百张级验证，但上万张时体积和重解码成本高，不能满足原图与连拍对比。最终选择 axum + 内嵌 vanilla JS：纯 Cargo、零 npm 工具链，浏览器负责图片交互，HTTP 与前端资产可在将来复用。

当时比较过 Tauri 2、egui/Slint 和 Dioxus；Tauri 桌面壳没有当前必要性，原生 GUI 缩略图墙/自由缩放成本较高，Rust UI 方案也增加开发和生态成本。因此只增加 Windows 原生选目录启动器，没有引入 WebView。未来是否增加壳需另作产品决定。

## 8. 后续记录规则

每轮追加日期、代码基线、问题、证据、处理结果与未验证边界；不要为不同模型或里程碑再创建一份评审文件。当前约定只在设计文档维护，用户可见变化只在版本说明维护。本文件中的旧实验不覆盖当前设计，也不作为未经确认的功能承诺。
