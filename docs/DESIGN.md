# firstcut — 当前设计与实现约定

> 当前设计对应 **v1.4.2**。
> 本文件是当前架构设计与实现约定的唯一维护入口；使用方法见 [README](../README.md)，版本变化见 [版本说明](RELEASE_NOTES.md)，漏洞报告见 [安全政策](../SECURITY.md)，历史评审、旧里程碑与已停止路线研究见 [历史归档](history/README.md)。

## 1. 产品边界与交付状态

firstcut 是 Rust 编写的本地照片初筛与复核工具。支持单目录或双卡/双目录（`jpg_root` + `raw_root`）扫描多品牌相机 JPG+RAW（索尼 `.arw`、尼康 `.nef`/`.nrw`、佳能 `.cr2`/`.cr3`/`.crw`、松下 `.rw2`、富士 `.raf` 及 `.dng`/`.orf`/`.pef`），分析 JPG 的清晰度、曝光、噪点、构图和美学，生成连拍候选，把结果映射给配对 RAW，输出 CSV 与供 Lightroom 使用的 XMP。工具不会删除、移动或修改照片像素；Web UI 对源照片目录/存储卡保持只读，分析与人工状态统一留在本机数据目录。

| 能力 | 当前状态与边界 |
|---|---|
| 评分与复核 | CLI 和本地 Web UI 均已实现，共用评分与星级合成；Windows 可双击启动 |
| 人工决定 | 人工星级、三态选片标记（留用/未定/剔除，含未评分照片）与连拍保留/舍弃覆盖均持久化在 `decisions.sqlite`，并统一作用于 review、CLI、CSV 与导出 |
| 场景 | 有五份手动选择的评分预设；自动提示仅是基于人脸的“人像候选”，没有通用场景分类或自动调权 |
| 图像 | 当前只支持 SDR JPEG 分析和直方图；RAW 不解码，仅配对与导出；HDR 增益图、PQ/HLG 未实现 |
| Windows 发行 | `pic_process.exe`、`firstcut-ui.exe`、`pic_process-gallery.exe`、`pic_process-tune.exe` 与 SHA256 校验文件 |
| Linux / Apple Silicon | GHCR 发布 Linux amd64/arm64 CPU 镜像；Apple Silicon 通过 Docker 运行 ARM64 Linux 镜像，原生 macOS 尚未列入发行范围 |
| 后期处理 | 曝光补偿仅作为 XMP 建议交接给 Lightroom；darktable 批量 RAW 开发路线已停止，不列入待办（见 [历史研究档案](history/DARKTABLE_PHASE2_RESEARCH.md)） |

模型文件单独准备，不打包进 Release。运行时模型缺失会提示并降级到纯像素模式，用户也可显式 `--no-ai`；分析来源必须可识别。历史吞吐数据只作参考，真实选片质量与 Lightroom 导入效果尚需单独验收。

## 2. 统一数据模型

| 层 | 内容与身份 | 责任 |
|---|---|---|
| 照片资产 | `PhotoEntry`；扫描时生成批次内 `pair_id`；人工星级以根目录下 JPG 的相对路径为键 | 扫描、EXIF、配对、CSV 行数据 |
| 分析结果 | `AnalysisResult`；五维子分、dHash、姿态描述子及曝光建议 | 可重建缓存，区分分析模式、曲线参数和模型文件来源 |
| 算法建议 | `selection::SelectionResult`；总分、算法星级、连拍簇/排名/保留建议 | 每次按当前配置和批次重新合成，CLI 与 review 共用 |
| 人工决定 | `EffectiveRating` 与独立决定库；最终星级 = 人工值优先，否则算法值 | 重评分不覆盖用户决定，统一用于 review、CLI、CSV 与 XMP |

人工星级、三态选片标记（`manual_flag`）与人工连拍覆盖同时记录相对路径与文件轻量内容指纹（文件大小 + 首尾各 64 KiB SHA-256），同一挂载点换卡后同路径的不同照片不会误继承旧卡的人工决定；首次导入旧版 `.firstcut/decisions.sqlite` 时会按原相册照片补齐指纹，无法确认来源的旧记录保留在库中但不自动应用；读取照片指纹失败时匹配阶段保留算法结果、保存阶段明确报错。移动或重命名照片、改变所选根目录后，相对路径可能变化，不承诺自动迁移。JPG 与其配对 RAW 使用同一最终星级与选片标记。XMP 是导出结果，不是人工决定的唯一存储；旧版仅写进 XMP 的人工星级不会自动导入决定库。

人工星级、三态选片标记与人工连拍保留覆盖统一保存在本机工作区 `decisions.sqlite`，并通过 `selection` 同步驱动 `review`、CLI `score`（CSV/XMP）与 `export`（包括 `export --picked` / `--flag` 及未评分照片的人工星级 XMP 导出）。

`analysis_mode` 标明 `ai` / `pixel`；`rating_source` 标明 `algorithm` / `manual`。模型整体不可用时可显式降级；已进入 AI 分析的单张推理失败应报告分析失败，不伪装成正常中性分。CSV 的 `analysis_ok` 可用于过滤失败或无配对 RAW；review 缓存没有失败记录表，无法完整区分“尚未评分”和“之前分析失败”。

## 3. 目录、配置与缓存

### 3.1 扫描及 JPG/RAW 配对

支持两种工作区形态：
- **单目录模式**：选一次拍摄活动的根目录，支持同目录 JPG+RAW，也支持 `JPG/`、`RAW/` 子目录布局。
- **双目录/双卡直连模式**：分别指定 `jpg_root` 与 `raw_root`（CLI `--raw-dir` 或 Web 欢迎页双目录标签），在只读挂载/双卡槽场景下直接配对。

配对按三级策略执行：（1）同相对子目录且主干名一致 `(rel_dir_lower, stem_lower)`；（2）剩余未配对中全局唯一的主干名 `stem_lower`；（3）仍不唯一时按 `(stem_lower, 非空 EXIF DateTimeOriginal)` 唯一匹配；仍存在歧义的不猜测，保持未配对。

“关联同名 RAW 照片”是工作区级偏好，UI、CLI `score` 和 `review` 共用；关闭后只处理 JPG。扫描跳过 `.firstcut/`、所选根下的 `Processed/` 输出子树，以及任何包含 `.firstcut-export` 标记文件的导出子目录；直接选择 `Processed/` 或导出目录本身则可以单独浏览。根目录顶层的成片 JPG 无法自动区分，建议单独放在 `Processed/`。

照片根统一规范化，Windows 去掉影响旧缓存匹配的 `\\?\` 路径形式。同一工作区不应因启动器、CLI 或进程工作目录不同而产生不同默认状态位置。

### 3.2 默认文件布局

所有运行期状态默认写入本机应用数据目录（Windows 为 `%LOCALAPPDATA%\firstcut\workspaces\<工作区ID>\`，Unix 为 `${XDG_DATA_HOME:-$HOME/.local/share}/firstcut/workspaces/<工作区ID>/`，可通过环境变量 `FIRSTCUT_DATA_DIR` 覆盖；单目录首次打开自动只读导入遗留 `<照片目录>/.firstcut/` 数据）：

| 本机工作区状态目录下路径 | 用途 / 保留方式 |
|---|---|
| `cache.sqlite` | 可重建的分析缓存 |
| `decisions.sqlite` | 统一人工决定库（人工星级 + 人工连拍保留/舍弃覆盖）；自动兼容并迁移旧版 `burst-overrides.json` |
| `scene-feedback.jsonl` | 场景纠正、备注与当时评分线索，需保留 |
| `config.toml` | 当前评分配置；不存在时使用内置默认值 |
| `ui-preferences.json` | RAW 关联和直方图抽样比例等工作区级偏好 |
| `workspace.json` & 上级 `recent-workspaces.json` | 工作区元信息（含上次导出目录）与最近工作区历史（最多 10 条） |
| `report.csv`、`scan-report.csv`、`metrics.csv` | 评分、扫描和调参报告 |
| `thumbs/v2/` | 按照片清单身份生成；8192 个固定槽、每槽最多 64 KiB，碰撞时可重新生成 |
| `review.log` | 评分与导出任务时间日志，严格限制为单文件 256 KiB |

导出与 XMP 规则：
- 单目录 CLI `score --xmp` 仍可在源目录照片旁写出 `<stem>.xmp`；Web UI 及双目录模式绝不向源目录/存储卡写 `.firstcut/` 或 `.xmp`。
- `export` 导出到目标目录时，若存在源 XMP（或已有人工决定），会在内存中无损合并最新人工星级后写入导出目录旁 `<stem>.xmp`（不修改源 XMP，也不覆盖已有 `crs:Exposure2012`）；目标根目录写 `.firstcut-export` 防二次扫描标记。

CLI 显式 `--cache`、`--output`、`--config` 可覆盖对应路径；旧工作目录缓存不自动猜测或搬移，可用 `--cache` 明确读取。`config-template` 没有照片根参数，默认写当前目录 `.firstcut/firstcut-template.toml`；gallery 默认输出到输入报告旁。

### 3.3 缓存与配置语义

缓存命中依据为相对照片根路径（兼容旧版绝对路径键）、文件大小、修改时间、`CACHE_VERSION`、配置曲线、分析模式与固定版本模型的 SHA-256 指纹（兼容旧版按本机模型 `mtime` 生成的缓存，再次评分时自动补写可移植行）。查找时对相对路径记录与旧版绝对路径记录**分别校验指纹**：若相对路径记录因旧配置等原因不匹配，仍会继续校验旧版绝对路径记录，不挡住仍有效的旧缓存。当前缓存版本为 15；它是实现常量，不是发行版本。缓存命中跳过解码与 AI，写入只更新 dirty 行。使用相对路径与固定模型哈希后，移动活动目录或将 Windows 已评分目录挂载到 Linux/macOS Docker 复核均可直接命中既有缓存，无本地模型时也能只读浏览既有 AI 评分。

> **搬迁与旧库迁移前提**：仍在旧版绝对路径键下的历史缓存库，须先在**原路径**跑一次评分完成相对路径行补写迁移，再移动目录或挂载进 Docker；此外，搬迁或复制照片文件时必须**保留文件修改时间（`mtime`）**（例如同卷移动、`robocopy`、`cp -p`、`rsync -t`），若复制重置了 `mtime`，文件指纹变化仍会触发重算。

权重、星级阈值和连拍参数在运行期重新合成建议，不应触发无必要的图片分析；清晰度/噪点/曝光曲线等缓存输入变化才重新分析。推理加载模型前独立执行固定 SHA-256 内存校验；`FIRSTCUT_MODEL_DIR` 指向软链父路径时会先规范化模型目录，再对目录内每个模型文件按无链接句柄打开并核对哈希。

显式配置文件不存在、未知字段、非法模式、NaN/无穷值、越界参数或错误的容差带顺序均须报错退出，不静默回退默认值。UI 保存前执行同一配置校验，使用 `toml_edit` 保留注释与字段顺序。默认权重和应为 1.0，校验容差为 ±0.05。

## 4. 评分、星级与连拍

### 4.1 五维分析

| 维度 | 默认权重 | 当前规则与取舍 |
|---|---|---|
| 清晰度 | 0.30 | 全局 Tenengrad 归一化与饱和映射；主体区域使用 reblur P80。SCRFD 主体脸 → 无可用主体脸时 pose 头部区域 → 都无则中性 50 下限；区域与全局取高者 |
| 曝光 | 0.25 | 极暗 ≤5 / 极亮 ≥250 像素比例惩罚 + EV 容差带；全图截尾均值排除最暗 25%，主体脸只向中灰方向修正 |
| 噪点 | 0.15 | 暗部 <40 的 8×8 块标准差 P15，取平滑暗块减轻纹理污染；ISO 容忍度 `k = 3.0 × (1 + 0.3 × log10(iso/100))` |
| 构图 | 0.15 | 主体脸三分法位置、大小和多人降权；主体脸高度 ≥4%，多人计数门槛为 5%；无人脸中性 60 |
| 美学 | 0.15 | CLIPIQA+，224×224、CLIP 归一化、sigmoid 输出 ×100 |

清晰度人脸区域半宽/半高为脸框尺寸 ×1.5，实际约 3× 脸框；pose 使用头部关键点包围盒 ×1.4/×1.6。区域与全局取最大值可以保护浅景深照片，但背景纹理也可能救回真实跑焦照片，这项取舍尚未改为只取主体分。

曝光目标默认 128；±1 EV（约码值 92–176）内满分，暗侧到 -4 EV、亮侧到 +2 EV 归零，两侧容差独立。主体级人脸中取最亮脸区域均值，只把判断值往中灰方向拉且不越过中灰，避免误检框压低正常照片分数。舞台、雪景等差异由预设配置表达，不为某批样片改全局默认值。

构图中脸高 8%–30% 为理想大小，小于 8% 线性降至 0.5，大于 30% 系数降至 0.75；三分法距离上限 0.47；多人系数为 1 人 1.0、2–4 人 0.95、5 人以上 0.85。当前预处理使用 `resize_exact`，尚未实现保长宽比的 letterbox；遮挡脸和极大脸仍可能漏检。

五个子分按实际配置权重合成总分。场景预设 `portrait`、`stage`、`highkey`、`sports`、`lowlight` 是用户手动选择的配置，不是逐张自动识别的结果。

### 4.2 星级

默认批次相对排名，0 表示最好：前 10% 为 5★，10%–30% 为 4★，30%–65% 为 3★，65%–90% 为 2★，其余为 1★。同分并列用平均位次，不拆分星级。小批次不保证出现所有星档；建议整场活动一次评分，跨批次比较应看同配置/模式下的总分。

`star_mode = "absolute"` 可切到绝对阈值，默认 75/60/45/30。人工星级优先于以上算法值；修改评分配置不能覆盖人工决定。

### 4.3 连拍候选

按 EXIF 时间排序，非负且 ≤2s 的相邻间隔聚为连拍组；组内 dHash（9×8 → 64 bit，汉明距离 ≤10）分近似子簇，再按 SCRFD 五关键点归一化的 10 维姿态描述子贪心聚类。姿态距离阈值默认 0.25；每个姿态簇按实际加权总分保留 top-K，默认 3 张，单组总保留上限 20。

无有效姿态描述子的帧共用伪簇；`adaptive_keep = false` 回退到 dHash 子簇保留；`-k` 显式值优先于配置。`burst_size` / `burst_rank` 对应保留单元，不一定是整组张数。姿态只影响候选，不改变五维分数和星级。保留/舍弃是建议或人工标记，不是文件删除操作；人工连拍保留覆盖统一存放在 `.firstcut/decisions.sqlite`（兼容迁移旧 `.firstcut/burst-overrides.json`），经 `selection.rs` 同步驱动 `score`（CSV/XMP）、`export` 与 `review`。

## 5. 处理链路与模块职责

```text
扫描/EXIF/配对 (scan) → 安全文件边界 (storage) → 缓存匹配 (cache)
    → JPG 解码与像素指标 (decode / metrics) → AI 主体与美学 (ai)
    → 统一结果合成 (selection / dedup / decision)
    → 导出与交互 (output/csv, output/xmp, export, review)
```

| 模块 | 职责 |
|---|---|
| `main.rs`、`bin/firstcut_ui.rs` | CLI 参数解析（`scan`/`score`/`export`/`config-template`/`review`）与 Windows 无控制台选目录启动器 |
| `storage.rs` | 基于 `cap-std` 句柄的安全目录与文件访问边界，逐级拒绝符号链接、重解析点、硬链接逃逸与未关闭数据库旁路日志，提供流式分块原子复制与卷剩余空间查询 |
| `scan.rs`、`decode.rs` | 目录扫描（跳过 `.firstcut`、`Processed/` 与含 `.firstcut-export` 的导出目录）、JPG/ARW 配对、EXIF 解析、JPEG 快速解码与安全兜底、EXIF 方向变换 |
| `metrics/`、`ai/`、`score.rs` | 五维像素指标、ONNX Runtime 推理（含固定 SHA-256 校验）、并行分析调度与 CLI/UI 共用跑批入口 |
| `cache.rs`、`config.rs` | SQLite 增量缓存匹配/写入、配置加载与严格校验、配置指纹与 5 份内置预设 |
| `dedup.rs`、`selection.rs`、`decision.rs` | 连拍分组与姿态聚类、最终星级与有效连拍决定合成、统一人工星级与连拍覆盖持久化 |
| `output/`、`export.rs` | CSV（默认表格公式防护 / `--raw-csv`）、XMP 侧车渲染/合并，以及筛选结果非破坏增量导出（分类型/扁平/保结构布局、索尼回绕冲突防覆盖、PS/ACR 侧车修改保护与取消安全） |
| `review/` | 本地 Web 复核服务（`mod.rs` 路由、`access.rs` 会话与来源鉴权、`snapshot.rs` 快照、`job.rs` 评分/导出单任务互斥与日志、`config_edit.rs`、`scene_feedback.rs`、`thumb.rs` 缩略图槽位缓存、`assets/` 内嵌前端） |
| `bin/tune.rs`、`gallery.rs`、`debug_*`、`probe.rs` | 调参指标导出、HTML 联系表生成与模型/像素诊断工具 |

JPEG 使用 `jpeg-decoder` 快速路径、`image` 兜底，全解码后 box 块平均到约 1024px 分析图；灰度 JPEG 走安全路径。EXIF Orientation 1–8 在降采样后应用，5/7 为先旋转后镜像。像素任务使用 rayon；AI 使用 ort 池化 Session/Mutex，当前单 Session 策略，历史实测增加池未改善性能。

三模型为 SCRFD 10g（640×640、阈值 0.3 与 NMS）、YOLOv8n-pose（640×640、输出 `[1,56,8400]`）和 CLIPIQA+。模型元数据、输出值域和通道布局应在换模型时重新验证。实验性 Windows DirectML 通过 `gpu` feature 和 `--gpu` 启用，默认 CPU；历史样本加速不显著。

## 6. XMP 与曝光建议

侧车命名固定为 `<stem>.xmp`。同目录 JPG/ARW 共用侧车，跨目录配对则两端各写一份同名侧车，星级一致。导出包含 `xmp:Rating`、firstcut 五维子分与连拍信息；曝光容差带外才给建议，按 1/3 EV 取整、默认钳位 ±2 EV，`exposure_suggest_cap = 0` 关闭建议。

`firstcut:suggestedEV` 是信息元数据，常规 `score --xmp` 与 UI 默认跑批仅写入该字段，不触碰 Adobe `crs:*` 开发参数。只有当用户显式开启 `--xmp-exposure`、`[metric] exposure_write_crs = true` 或在 UI 勾选“写入建议曝光到 Adobe 开发参数”时，才会在尚无已有 `crs:Exposure2012` / `crs:Exposure` 的侧车中写入 `crs:Exposure2012`（搭配 `crs:HasSettings="True"` 与 `crs:ProcessVersion="11.0"`，且不降级已有更新的 `ProcessVersion`）；程序不修改照片像素。`POST /api/rate` 改星对已有 firstcut 侧车仅更新 `xmp:Rating`，不改动 `firstcut:suggestedEV` 或任何 `crs:*` 字段。

含 firstcut 命名空间的已有侧车仅合并受管理字段，兼容元素写法与 Adobe XMP SDK 规范化后的属性/自闭合标签写法，开启建议曝光写入时自动清理早期误写的 `crs:Exposure2`，但**绝不覆盖或删除侧车中已有的 `crs:Exposure2012` / `crs:Exposure` 人工曝光及其他外部 CRS 调整**（即便新算出的建议曝光为空）。无 firstcut 标记的侧车不覆盖，解析失败保护原文件。用户在 Lightroom 中改过的 firstcut 管理字段（如 `xmp:Rating`）仍可能在再次导出时被更新，不能承诺保护这些字段的后续人工修改。

人工决定保存与侧车写入不是同一个事务：XMP 同步失败应提示，但已保存的人工星级仍有效。真实 Lightroom 导入与曝光效果尚未验收，不能用 XML 测试替代。

## 7. 本地 Web UI 与交互约定

选择 axum + 内嵌 vanilla JS，纯 Cargo 构建，无 npm/WebView 依赖。CLI `review` 与 Windows 启动器共用服务；启动后先显示页面，再异步读取快照，报告扫描阶段、计数与可确定总量时的进度，完成后自动刷新。评分缺失时提示选择原评分目录或继续未评分浏览，不强制重算。

- 缩略图墙分页懒加载，筛选和排序（总分、拍摄时间新→旧/旧→新、文件名 A→Z/Z→A）自动记忆；默认按连拍组聚合，每组显示当前排序最靠前的一张，指定组后显示全部。卡片支持留用/剔除角标与快捷按钮，侧栏支持按选片标记筛选并一键将当前筛选批量设为留用；照片右键可快速设置选片标记、筛组及设置保留覆盖。
- 星级、分数下限及说明可折叠，桌面侧栏可拖宽，双击边界复位，窄屏可收起。筛选、折叠状态及面板位置等部分设置保存在浏览器 localStorage。
- 灯箱支持原图缩放/拖动、双击复位、`←/→` 切换（支持顶部工具栏与界面设置切换「跨组切换 / 锁定连拍组」，默认跨组）、有界内存 LRU 预加载同组/邻近原图、2–4 张并排对比与百分比显示；对比共用图像归一化锚点及缩放状态；支持快捷键 `P`/`X`/`U` 标记留用/剔除/未定与 `1–5` 改星（未评分照片也可直接标记与打星）。EXIF 光圈/快门/焦段用 `f/`、`s`、`mm` 明确标识。
- 评分面板跟随活动照片，可拖动和调整大小；列出子分、实际权重与贡献。人像候选只是规则线索；用户可保存真实场景与备注，供后续校准，不自动切换评分参数。
- 配置表单提供说明、建议范围、滑块与输入框，不把 M9、M-UI2 等内部规划编号暴露在产品文案中。
- UI 内跑批使用 `score::run_score_job`，全局单任务互斥；轮询进度与日志，完成后热替换快照。跑批期间限制改星和服务控制，避免竞争。
- Windows 启动器页面提供选目录、RAW 关联、重启与退出；关闭浏览器不等于停止服务，再次双击可打开仍在运行的页面。CLI review 使用 Ctrl+C 退出。

### 7.1 直方图与性能边界

半透明可拖动面板默认位于图像文件名下方，可显示/隐藏；W/R/G/B 通道可分别开关，并跟随对比中的活动照片。横轴为解码后 JPEG 8-bit 码值 0–255，纵轴为当前可见通道共同峰值归一化的相对频数；W 用 `0.2126R + 0.7152G + 0.0722B` 近似亮度，不是物理亮度单位或 HDR nits。

浏览器以关闭平滑的 Canvas 最近邻缩图近似网格抽样，默认 10%，照片根级设置允许 1%–25%。只缓存在内存（最多 8 个结果），不生成直方图文件。它独立于后端约 1024px 的评分分析图，不因调整直方图比例而改变评分。

抽样比例主要影响 Canvas、RGBA 读取及逐像素计数，开销随样本数近似线性；不会同比缩短原图解码耗时。33MP 图像的 10% 约 330 万采样点，单份 RGBA 像素数组约 13.2 MB（12.6 MiB），还需 Canvas、原图解码等额外内存，不能当作整个浏览器的峰值。10% → 25% 的采样阶段工作量约为 2.5 倍；这些是规模估算，没有跨设备耗时保证。固定网格可能漏掉少量极值，暗部/亮部百分比仅作估计，不能替代精确裁切检查；当前仅适用于 SDR JPEG。

### 7.2 日志与接口

任务日志带本地时间，页面显示有界缓冲，落盘仅 `.firstcut/review.log`；上限 256 KiB，超限保留最近约 128 KiB，超长单行截断，不无限叠加轮转文件。

HTTP 端点：
- **公开与鉴权**：`/`（页面框架）、`/review.js`（内嵌脚本）、`/healthz`（启动器探活）、`POST /api/session`（凭据换会话）
- **数据与图像（需会话）**：`GET /api/photos`、`GET /api/load`、`GET /thumb`、`GET /image`
- **任务、标注与配置（需会话 + 写接口来源校验）**：`GET /api/job`、`POST /api/score/run`、`POST /api/export/pick-folder`、`POST /api/export/preview`、`POST /api/export/run`、`POST /api/export/cancel`、`POST /api/export/open`、`POST /api/rate`、`POST /api/flag`、`POST /api/flag/batch`、`POST /api/burst/keep`、`POST /api/scene-feedback`、`GET/POST /api/config`、`GET/POST /api/ui-settings`、`GET/POST /api/control`

## 8. 安全、验证与发布

默认只监听 `127.0.0.1:8787`。复核链接中的访问凭据位于 URL fragment，不进入普通 HTTP 请求地址；页面兑换服务会话后清除链接片段。`POST /api/session` 具备滑动窗口失败限速（60 秒内连续失败 5 次返回 `429 Too Many Requests` 与 `Retry-After`）。会话按浏览器的协议、主机名和端口保存，由请求头显式发送，完整进程退出后失效。Windows 双击启动器通过当前登录会话内命名事件（`Local\firstcut-ui-reopen-8787`）唤起已有进程重开带凭据链接，不在磁盘或 HTTP 暴露凭据。访问校验在路由统一处理：公开页只返回页面框架与脚本，图片与照片元数据需要有效会话；写接口还要求匹配服务地址的 Origin 和 JSON 请求体，并通过统一的取消安全执行器将异步写互斥锁（`OwnedMutexGuard`）、`JobState` 写守卫（与跑批、导出及服务关闭互斥）及 `AppState` 一并移入 `spawn_blocking` 任务——即使客户端中途断连取消 HTTP 请求，后台任务仍会持锁并持有守卫直到落盘、内存状态与快照全部更新完成。Web UI 导出相册外目录仅允许本机 Windows 桌面回环模式通过原生文件夹弹窗授权并由服务端颁发不透明 `target_id`（不接受浏览器任意外部路径，且打开资源管理器仅限服务端记录的最近导出目录）；容器/远程监听模式（`--bind 0.0.0.0`）仅允许相册内独立子目录（且拒绝相册根、`.firstcut` 与已有原片目录）。响应禁止第三方站点加载；原图只按当前快照的 JPG 身份读取并流式传输。

`.firstcut/`、自动导出的 XMP 与相册照片通过打开的目录句柄定位（`storage.rs`，Windows 上对相册前缀按大小写不敏感归属判定），逐级拒绝符号链接、Windows 重解析点（含 OneDrive 云占位目录）与有多条硬链接的文件。数据库从安全打开的文件读入，SQLite 不直接操作相册内的文件名；拒绝未关闭或有待恢复日志的库，保留原件。普通状态以新文件完整写入后原子替换；日志与场景反馈受大小和文件身份约束。场景反馈达到 16 MiB 时拒绝继续追加并提示归档。分析限制为单图 128 MiB、1.1 亿像素和进程内最多两次并行解码；原图接口发送体积同样上限 128 MiB。

HTML 联系表转义所有元数据，只允许运行固定的排序脚本；读取的 JPG 必须位于选定照片根内。面向表格使用的 CSV 默认保护可能被当作公式的文本，显式 `--raw-csv` 留给程序读取。推理仅加载 [README](../README.md#模型准备一次性) 中固定 SHA‑256 的模型。

容器显式监听 `0.0.0.0` 接收 Docker 转发，必须配置访问凭据；Compose 将宿主端口限定在 `127.0.0.1`，将宿主机 `DATA_DIR` 挂载至 `/app/data`（`FIRSTCUT_DATA_DIR=/app/data`）持久化工作区状态，并通过 `FIRSTCUT_WORKSPACE_KEY=${PHOTO_DIR}` 按宿主机相册路径隔离不同相册。`PHOTO_DIR` 挂载至 `/photos`，`MODEL_DIR` 只读挂载至 `/app/models`。运行用户映射到宿主 UID/GID。构建上下文按源码清单纳入文件，排除本地 `memory.md`、照片和状态。

| 工作流 | 触发与交付 |
|---|---|
| CI | main push / PR，Windows、Linux x64/ARM64 测试、构建与 RustSec 依赖审计；Windows 发行程序使用合成照片验证登录、取图和评分，Linux 双架构镜像以归档执行静态漏洞扫描 |
| CodeQL | main push / PR、定时或手动，扫描 Rust 与 Actions |
| Dependabot | 提出依赖更新 PR；须验证配置/XMP/缓存兼容性，不能只凭版本号合并 |
| Release | 标签必须指向通过 CI 和 CodeQL 的 main 提交；核对 Cargo 版本，构建 Windows 程序与 SHA256，两架构镜像通过静态扫描后按摘要发布，附构建证明；手动触发只留临时工件 |

发布前对齐 `Cargo.toml` / `Cargo.lock` / [`RELEASE_NOTES.md`](RELEASE_NOTES.md)，main CI 通过后推 tag。源码、文档、无敏感内容的测试可以提交；私人照片、真实本地测试集、模型、凭据与本地沟通记录不得发布。

涉及行为变化时运行适当测试；完整测试命令为 `cargo test --locked --all-targets`，前端脚本执行 `node --check src/review/assets/review.js`。记录平台、命令、退出码、跳过项及真实图库验收范围，不把旧测试数当当前实时结果。缓存值语义变化应递增 `CACHE_VERSION`，仅权重/阈值变化不应无谓清空缓存。代码改动同步更新 [README](../README.md) 与本文件，历史评审只追加到 [`history/REVIEWS.md`](history/REVIEWS.md)。

## 9. 后续工作与未验收项

| 优先顺序 | 方向 | 理由与验收重点 |
|---|---|---|
| 1 | 选片手感与人像工作流 | 一键 1:1 居中定焦人脸/眼睛查合焦、粘贴文件名清单反选 RAW、闭眼/表情废片过滤 |
| 2 | 建立真实选片标注集 | 检查好片被排除率、每组最佳帧候选覆盖率，再决定权重/阈值优化；单次“界面可用”不能代表算法质量验收 |
| 3 | Lightroom / ACR 实际导入 | 核对 JPG/ARW 配对、星级、firstcut 字段、导出目录布局和 `crs:Exposure2012` 的实际显示及效果 |
| 4 | 模型输入和主体可靠性 | letterbox/坐标反算、遮挡和超大脸、主体分策略均需检出率与选片回归，不能无验证直接替换 |
| 待单独设计 | 原生 macOS / HDR | Apple Silicon 原生需验证 CoreML 与发行链路；HDR 需设计色彩管理、亮度域及增益图语义，不能把 SDR 直方图直接当 HDR 支持 |

旧 M 系列编号只用于查历史；已停止的 darktable 编排器与旧 GUI 验证清单不在本表计划内。
