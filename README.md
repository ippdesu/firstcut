# firstcut — 索尼照片初筛评分工具

用 Rust 编写的本地照片初筛工具：扫描索尼相机 JPG+ARW 目录，对 JPG 做
**五维评分**（清晰度 / 曝光 / 噪点 / 构图 / 美学），连拍去重排序，输出
**CSV 报告**和 **XMP 星级侧车**。全程本地运行、照片不上传。

> 当前状态：**v1.2 已发布**，提供可用的本地照片复核界面、评分解释、筛选与人工决定，并统一 CLI、CSV、XMP 的星级来源。
> 当前方向：本地初筛、人工复核、Lightroom 元数据交接。darktable 批量 RAW 开发路线已停止；历史调研保存在 `DESIGN.md` §9。

## 构建

```bash
cargo build --release
# 可选：实验性 DirectML GPU 推理（score --gpu；实测无显著收益，见 DESIGN §8）
cargo build --release --features gpu
```

产物（`target/release/`，Windows 加 `.exe`）：

| 二进制 | 用途 |
|---|---|
| `pic_process` | 主命令（`scan` / `score` / `config-template` / `review`） |
| `firstcut-ui` | Windows 无控制台启动器：双击选目录，自动启动本地复核 UI；页面内可重启或退出服务 |
| `pic_process-tune` | 调参工具：导出原始指标 CSV |
| `pic_process-gallery` | HTML 联系表生成器（缩略图 + 分数） |
| `pic_process-debug-pose` | 诊断工具：YOLOv8-pose 检测验证 |
| `pic_process-debug-scrfd` | 诊断工具：SCRFD 检测验证（含 kps/姿态描述子） |
| `pic_process-debug-exposure` | 诊断工具：全图亮度 vs 主体脸亮度 |
| `pic_process-probe` | 诊断工具：打印 ONNX 输入输出元数据 |

> `pic_process.exe` 主二进制已静态链接 onnxruntime，单文件免 DLL；
> 辅助二进制也随 release 一同构建，可按需取用。

### 双击启动复核 UI（Windows）

运行发行包里的 `firstcut-ui.exe`，在目录窗口中选择照片根目录后会自动启动本地服务并打开浏览器。若 JPG 和 RAW 分放在 `JPG/`、`RAW/` 子目录，请选择同时包含这两个目录的活动根目录；只有 JPG 时可直接选择 JPG 目录。复核 UI 会递归扫描所选目录，但默认跳过其中名为 `Processed/` 的输出子目录；若要看成片，可直接选择 `Processed/` 本身。放在活动根目录顶层的 JPG 无法与原片自动区分，也会进入列表，建议把成片放进 `Processed/`。取消选目录会直接退出。侧栏的“选择照片目录…”可随时换目录；“关联同名 RAW 照片”开关可决定是否载入 RAW、把 JPG 分数映射到 RAW 并在评分或改星时写其 XMP 侧车。关闭后只处理 JPG，该偏好保存在所选根目录的 `.firstcut/ui-preferences.json`；CLI 默认仍关联 RAW。启动器使用所选根目录下的 `.firstcut/config.toml` 和 `.firstcut/cache.sqlite`；首次评分可在 UI 侧栏点击“重新评分”。

浏览器会在启动后立即显示实际扫描阶段、已发现或已读取文件数，以及有总数时的进度条；完成后自动切换照片列表。若目录已有 `.firstcut/report.csv`，复核会复用其中未变照片的 EXIF 元数据来加快扫描，评分仍以 `cache.sqlite` 中的文件指纹和分析记录为准。Windows 启动器把系统返回的 `\\?\` 路径规范化为缓存中的普通绝对路径，以免同一目录被误判为不同目录。未找到可用评分时，页面会提示重新选择之前评分的照片目录，或继续按未评分照片浏览。启动器会从可执行文件所在目录及其上级目录寻找 `models/`，因此在项目内双击 `target/release/firstcut-ui.exe` 也能命中原有 AI 评分缓存。GitHub Release 不包含模型文件；独立使用发行包时，请把模型放在可执行文件同级的 `models/` 目录。

页面右上角的“重启服务”会重新读取配置与照片快照，“退出程序”会关闭本地服务并释放 8787 端口。评分运行时这两项操作会被拒绝，需等评分完成。仅关闭浏览器标签页不会停止服务；再次双击 `firstcut-ui.exe` 会重新打开仍在运行的页面。CLI 的 `review` 模式继续使用终端里的 Ctrl+C 退出。

命令行用户继续运行 `pic_process review <照片目录>`，原有 CLI 流程保持不变。

## 自动检查与发布

GitHub Actions 的 [CI 工作流](.github/workflows/ci.yml) 在提交到 `main`、向 `main` 提交 PR 时运行 Windows 测试和发行构建，也可手动运行。[Release 工作流](.github/workflows/release.yml) 在推送版本标签时重新测试、构建发行程序，生成 SHA-256 校验文件并创建 GitHub Release。手动运行 Release 工作流只构建并保存 14 天的临时工件，不会发布新版本。

发布新版本时，先将 `Cargo.toml` 中的版本号和 `Cargo.lock` 更新并合入 `main`，确认 CI 通过，然后推送对应标签。例如 `1.3.0` 使用 `v1.3` 或 `v1.3.0`。发布任务会核对标签与版本号，GitHub 自动生成发行说明。`v1.2` 是此前手动发布的版本；这套自动流程从下一个标签开始使用。模型文件和本地照片不会进入发行包。

## 模型准备（一次性）

`score` 需要 `models/` 目录下三个模型（已 gitignore）：

| 文件 | 来源 | 大小 |
|---|---|---|
| `clipiqa_model.onnx` + `.onnx.data` | [86Cao/IQA-ONNX-Models](https://huggingface.co/86Cao/IQA-ONNX-Models)（CLIP-IQA+，learned prompts 烘焙进模型） | ~153MB |
| `scrfd_10g_bnkps.onnx` | [RuteNL/SCRFD-face-detection-ONNX](https://huggingface.co/RuteNL/SCRFD-face-detection-ONNX)（InsightFace SCRFD 10g，小脸检测强） | 16.9MB |
| `yolov8n_pose.onnx` | [Xenova/yolov8n-pose](https://huggingface.co/Xenova/yolov8n-pose)（人体姿态，人脸漏检时定位头部） | 13.5MB |

模型缺失时 `score` 自动降级为纯像素评分并提示；`--no-ai` 可显式跳过。CSV 的 `analysis_mode` 标明本次是 `ai` 还是 `pixel`；两种结果使用不同缓存指纹，模型文件变化也会使 AI 缓存失效。
下载后放到 `models/` 即可，无需 `download-models` 子命令。

## 用法

```bash
# 只建索引（EXIF + 配对，不评分）
pic_process scan <照片目录> -o report.csv

# 评分 + 连拍去重（推荐）
pic_process score <照片目录> -o report.csv

# 评分 + 写 XMP 星级侧车（Lightroom 可读）
pic_process score <照片目录> --xmp

# 本地 Web 复核界面（缩略图墙 / 1:1 原图 / 连拍并排对比，浏览器打开）
pic_process review <照片目录>

# 增量重跑（SQLite 缓存，只处理新照片/变更照片）
pic_process score <照片目录>            # 第二次几乎秒级

# 其他选项
pic_process score <目录> -k 2           # 每个保留单元保留 2 张
pic_process score <目录> --no-ai        # 跳过 AI 推理
pic_process score <目录> --no-cache     # 禁用缓存
pic_process score <目录> --cache x.db   # 指定缓存文件
pic_process score <目录> --config x.toml # 自定义评分配置（多场景可存多份）
pic_process score <目录> --gpu          # 实验性 DirectML（需 --features gpu 构建）

# 生成评分配置模板
pic_process config-template -o firstcut.toml

# 生成内置场景预设（portrait / stage / highkey / sports / lowlight）
pic_process config-template --preset stage -o stage.toml

# 辅助工具
pic_process-gallery report.csv -o gallery.html   # HTML 联系表（缩略图+分数）
pic_process-tune <目录> -o metrics.csv           # 原始指标（调参用）
```

## 多场景配置

不同拍摄场景用不同权重与曝光容差。内置 5 个场景预设，可直接生成后微调：

```bash
pic_process config-template --preset stage -o stage.toml
pic_process score <目录> --config stage.toml
```

| 预设 | 适用场景 | 主要差异 |
|---|---|---|
| `portrait` | 人像 / 漫展 / 棚拍 | 等同于内置默认值 |
| `stage` | 舞台演出 / 暗厅 / live | 曝光权重 0.20、暗侧容差放宽到 -2 档、构图权重 0.20 |
| `highkey` | 白背景 / 白裙 / 雪景 | 目标亮度 150、亮侧容差放宽到 +3 档 |
| `sports` | 运动 / 打鸟 / 飞机 | 清晰度权重 0.45 且判定更严格、构图权重 0.05、关闭主体感知曝光 |
| `lowlight` | 夜景 / 暗光室内 | 目标亮度 110、暗侧容差 -5 档、噪点更宽容、美学权重 0.20 |

`config-template`（不带 `--preset`）输出的是带完整中文注释的通用模板，
只写想改的字段即可。**换了 `--config` 无需换缓存文件**：配置指纹已并入缓存键，
配置变化会自动重算（见下）。

> 配置是 **fail-fast** 的：`--config` 指向的文件不存在/解析失败会直接报错退出
> （不再静默回退默认值），未知字段名也会被拒绝——防止手滑的字段名静默失效。
> 但注意：改**权重**或**星级阈值**不会触发重新分析（它们不影响缓存里的五维子分），只会重新合成总分和星级；改曲线参数（`sharpness_k`/`noise_k0`/`exposure_*`）会重新分析。`NaN`、无穷值和越界参数会被拒绝。

## 输出说明

**CSV**（`report.csv`）每行一张照片（ARW 分数映射自同名 JPG）：

| 列 | 含义 |
|---|---|
| `path, filename, extension, is_raw, has_pair` | 文件标识与 JPG/ARW 配对状态（JPG 与 ARW 同目录配对；分放 `JPG/`+`RAW/` 时同名且无歧义也可配对） |
| `date_time_original` | EXIF 拍摄时间（连拍聚类用） |
| `camera_make, camera_model, lens_model` | 相机/镜头 |
| `iso, f_number, shutter_speed, focal_length` | 曝光参数 |
| `sharpness_score, exposure_score, noise_score, composition_score, aesthetic_score` | 五维子分（0-100） |
| `total_score` | 加权总分（0-100，跨批次可比） |
| `stars` | 星级 1-5（默认按**本批次相对排名**，见下） |
| `rating_source` | `algorithm` 为算法建议；`manual` 为人工星级。人工决定优先，重跑不会覆盖 |
| `analysis_mode` | `ai` 为完整模型分析；`pixel` 为纯像素分析或模型不可用时的降级结果 |
| `faces` | SCRFD 检测到的人脸数 |
| `analysis_ok` | 评分数据是否可用（解码失败/无配对 ARW 为 false，运行结束 stderr 也有失败清单） |
| `burst_group, burst_size, burst_rank, burst_keep, burst_pose` | 连拍去重：组号、**保留单元内**张数、保留单元内排名、是否建议保留、姿态簇号（`burst_keep` 只是建议标记，**工具永不删除/移动文件**） |

**XMP 侧车**（`--xmp`）：写 `<stem>.xmp`（如 `DSC00001.xmp`），含
`xmp:Rating`（1-5 星）+ `firstcut:` 命名空间（五维子分/人脸/连拍信息）。
同目录的 JPG/ARW 共用一个侧车；分放不同目录时各写一份同名侧车，使用相同的最终星级。
**已有其他软件写的侧车不会被覆盖**（只提示跳过）。firstcut 创建的侧车若后来加入其他字段，重跑仅合并 firstcut 管理的分数、星级和曝光建议字段，保留其余 XML 内容；文件无法解析时保留原文件并报告写入失败。

> 命名兼容性：`<stem>.xmp` 是 **Lightroom / Camera Raw** 的约定，**darktable 也读**
> 这种格式（它自己的 `<stem>.<扩展名>.xmp` 也认）。所以一份侧车两边都能用。

## 评分维度（默认权重，总和 1.0）

| 维度 | 权重 | 方法 |
|---|---|---|
| 清晰度 | 0.30 | 主体感知三层链路：SCRFD 人脸区域 reblur P80（半宽/半高 = 脸框尺寸 ×1.5，即约 3× 脸框）→ 无主体级人脸时 YOLOv8-pose 头部关键点区域 reblur P80 → 都无则 50 分中性下限（大光圈浅景深照片不会被误判）；区域分与全局分**取高者** |
| 曝光 | 0.25 | 过曝/欠曝像素比例（4× 惩罚）+ 判定亮度偏离理想值的 **EV 容差带**（默认 ±1 档内满分，-4 档 / +2 档降为 0）；判定亮度在有主体级人脸时用主体脸亮度做单向修正 |
| 噪点 | 0.15 | 暗部 8×8 块标准差 P15（最平滑暗块）+ ISO 容忍度曲线 `k = 3.0·(1+0.3·log10(iso/100))` |
| 构图 | 0.15 | SCRFD 主体级人脸（高度 ≥ 4%）：三分法位置 + 主体大小（8%~30% 理想）+ 多人降权；无主体脸时中性 60（不惩罚风景/静物） |
| 美学 | 0.15 | CLIPIQA+（CLIP 底座，sigmoid 输出 ×100 → 0-100 分） |

> 加载 `--config` 时，权重和需在 1.0±0.05 范围内；曝光容差带必须严格嵌套
> （`exposure_ev_lo > exposure_ev_full_lo`），否则拒绝加载。

### 曝光为什么用 EV 容差带

用"平均亮度偏离中灰多少"打分，会把两类**合法**场景误判成曝光失误：
舞台黑幕布（全图均值被拉低）、白裙白背景（全图均值被拉高）。所以：

1. **容差带按曝光档位（EV）定义**，而不是按码值。±1 EV 对应码值 92~176，
   是 AE 的正常波动范围；超出后线性衰减，暗侧到 -4 EV、亮侧到 +2 EV 降为 0
   （亮侧更陡：高光溢出在 JPG 里不可恢复，暗部在 RAW 里通常还能救）。
2. **两侧容差独立**：舞台要放宽暗侧但不能放宽亮侧，雪景反之。
3. **主体感知做单向修正**：有主体级人脸（高度 ≥ 4%）时，取最亮的一张主体脸
   亮度，把判定值往中灰方向拉（最多拉到中灰，不会越过）。所以暗色/亮色误检框
   不会把正常照片打下去，同时暗背景/亮背景场景能被救回。

**星级分档**：默认 `star_mode = "relative"`，按**本次批次内的相对排名**给星：

| 星级 | 批次内百分位（0 = 最好） |
|---|---|
| 5★ | 前 10% |
| 4★ | 10%~30% |
| 3★ | 30%~65% |
| 2★ | 65%~90% |
| 1★ | 后 10% |

为什么不用绝对阈值：各维度为了防止误杀都带中性地板（清晰度 50 保底、构图无主体
60、曝光容差带），实测一批 119 张按绝对阈值全部落在 4~5 星，星级就失去了筛选作用。
**总分仍原样写进 CSV/XMP**，跨批次比较看分数而不是星级。
同分并列取平均位次，不会被拆成不同星级。
想要绝对阈值就设 `star_mode = "absolute"`（阈值 `rating_5..2`，默认 75/60/45/30）。

> 注意：星级依赖"批次"——建议**整场照片一次跑完**。分批跑不同子目录会各自归一化，
> 星级之间不可比。
> 人工改星保存在照片根目录的 `.firstcut/decisions.sqlite`，优先于算法星级，并同步给配对的 JPG/ARW。此文件是用户决定，**备份照片目录时请保留**；评分缓存 `pic_process_cache.sqlite` 可删除后重建。旧版仅写入 XMP 的人工星级不会自动导入决定库，需要在复核界面重新确认。

**连拍去重（默认按姿势分组保留）**：先按 EXIF 拍摄时间排序，再以间隔 ≤2s 成组 → 组内按 dHash
汉明距离 ≤10 分**子簇**（近乎同一张）→ 子簇内再按 **SCRFD 关键点姿态描述子**
聚类（距离 > 0.25 = 不同姿势）→ **每个姿势簇各自保留 top-3**（`-k` 可调），
单组保留总量受上限 20 约束（超出按总分截断）。
解决"30fps 连拍不同姿势被 dHash 判为近似重复只留 2 张"的问题；
无人脸的帧退化为 dHash 行为；`burst_keep=true|false` 只是建议保留标记。
阈值/开关/上限都在 `[dedup]` 配置段（`adaptive_keep = false` 回退旧行为）。

## 本地复核与操作台（review）

```bash
pic_process review <照片目录>            # 浏览器自动打开 http://127.0.0.1:8787
pic_process review <目录> --port 9000 --config stage.toml
```

- **缩略图墙**：分页 + 懒加载，卡片显示星级/总分/连拍保留标记；筛选（星级、
  保留/未保留、五维分数下限、有人脸、文件名搜索）与排序，条件自动记忆。
- **1:1 原图灯箱**：点击卡片打开原图（按需直读原文件，不预生成），
  滚轮缩放、拖拽平移、双击复位，`←/→` 在同一连拍组内切换——缩略图看不清
  是否合焦时随时放大到 100%。
- **连拍组并排对比**：灯箱内勾选同组 2~4 张，并排窗格**同步缩放平移**
  （滚轮/拖拽作用于所有窗格），逐帧对比合焦位置。
- **评分说明与场景标注**：打开照片后，悬浮面板列出五项子分、实际权重和各自贡献。并排对比时可点击窗格或在面板中选择照片；面板标题可拖动，右下角可调整大小。场景初判目前仅依据人脸检测给出“人像候选”，其余显示“未识别”；这不是通用场景分类器，也不会自动切换评分配置。可选择实际场景并写备注，记录追加到 `.firstcut/scene-feedback.jsonl`，供后续识别与评分校准。
- **手动调整连拍保留标记**：照片右键可设为保留/舍弃，也可恢复自动建议；标记保存在照片根目录的 `.firstcut/burst-overrides.json`，复核界面重启后仍有效。
- **UI 内跑批**：侧栏"重新评分"触发完整评分流水线（与 `score` 子命令
  同一实现），进度条 + 日志实时可见，完成后快照自动刷新，无需重启服务。
- **UI 内改星**：灯箱内点星级或按 `1~5` 快捷键 → 保存人工决定并同步 XMP。
  重启复核界面、重新评分或导出后仍使用人工星级；他人侧车不覆盖。侧车同步失败时，界面会提示，人工决定仍然保存。
- **配置编辑**：权重/星级阈值/EV 容差/[dedup] 表单化编辑，
  保存保留 TOML 注释，非法值（权重和越界等）拒绝写盘。
- 默认 UI 配置位于照片根目录的 `.firstcut/config.toml`，再次打开 review 会自动加载；UI 跑批报告位于 `.firstcut/report.csv`。CLI 使用 UI 配置时传 `--config <照片目录>/.firstcut/config.toml`。
- 数据来源：扫描目录 + SQLite 分析缓存 + 独立的人工决定库（星级/连拍与 `score` 同一逻辑在线计算）。
  未跑过评分的照片显示为未评分。

## 性能（16 核机器实测）

- 119 张 33MP 冷缓存（含 AI）：约 **146ms/张**（CLIPIQA + SCRFD + 姿态）
- **1447 张真实图库冷跑 2m55s（~121ms/张，88% 连拍）**；增量重跑 1447 张全命中 2.06s
- 实验性 `--gpu`（DirectML）：119 张 16.7s vs CPU 17.4s——无显著收益，保持实验性
- 纯像素冷缓存：约 **90ms/张**（`--no-ai`）
- 增量重跑：秒级（SQLite 缓存，键 = `path` + `size` + `mtime` + `CACHE_VERSION` + 配置/分析模式/模型文件指纹）
- 评分参数/`--config` 变更会自动使缓存失效（配置指纹参与缓存键），无需手动换缓存文件

## 目录结构

```
src/
├── main.rs        # CLI（scan / score / config-template / review）
├── lib.rs         # 库入口（pub mod 各模块）
├── scan.rs        # 目录扫描 + EXIF + JPG/ARW 配对
├── decode.rs      # JPEG 解码（box 降采样）+ EXIF 方向 + 灰度/直方图
├── metrics/       # sharpness / exposure / noise / composition
├── ai/            # CLIPIQA + SCRFD（含 kps）+ YOLOv8-pose（ort 推理）
├── dedup.rs       # 连拍分组 + dHash 与姿态聚类 + 排序
├── cache.rs       # SQLite 增量缓存（键含配置指纹，含姿态描述子）
├── decision.rs    # 独立的人工星级决定与最终星级合成
├── selection.rs   # CLI/review 共用的星级与连拍结果合成
├── review/        # 本地 Web 复核服务（axum + 内嵌前端）
├── output/        # csv / xmp 侧车
├── config.rs      # 权重与曲线参数 + [dedup] + 场景预设（TOML 可配）
└── bin/
    ├── tune.rs             # 原始指标导出（调参用）
    ├── gallery.rs          # HTML 联系表生成器
    ├── debug_pose.rs       # 诊断：YOLOv8-pose 检测
    ├── debug_scrfd.rs      # 诊断：SCRFD 检测 + kps 自检 + 姿态描述子
    ├── debug_exposure.rs   # 诊断：全图亮度 vs 主体脸亮度
    └── probe.rs            # 诊断：ONNX 模型元数据

presets/                  # 场景预设（编译进二进制，config-template --preset 输出）
├── portrait.toml  stage.toml  highkey.toml  sports.toml  lowlight.toml
```

## 测试

```bash
cargo test --locked
# 前端脚本检查：将 index.html 中的 <script> 内容提取到临时 .js 文件后执行 node --check
```

测试覆盖扫描和配对、分数与连拍、配置边界、灰度 JPEG、XMP 字段保留，以及“评分 → 人工改星 → 重跑 → CSV/XMP/快照”流程。少数旧集成测试依赖 gitignore 中的私人 `testpic/`；该目录不存在时会跳过这些用例。

## 相关文档

- [`DESIGN.md`](DESIGN.md) — 当前设计约定、技术选型及已停止路线的历史记录
- [`release_notes.md`](release_notes.md) — 版本说明
- [`REVIEW.md`](REVIEW.md) — 外部评审记录与处理状态（长期累积，每轮评审追加）
- [`ASTRA_REVIEW.md`](ASTRA_REVIEW.md) — 本轮评审的过程、实施结果、验证和后续建议

## 方向与后续工作

当前产品聚焦选片与复核；曝光补偿只作为 Lightroom 侧车建议输出。`DESIGN.md` §9 中的 darktable 批量开发方案已停止，属于历史调研。下一步是用用户真实选片结果校准“误删好片”和连拍候选覆盖率；这项校准尚未完成。
