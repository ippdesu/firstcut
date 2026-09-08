# firstcut — 索尼照片初筛评分工具

用 Rust 编写的本地照片初筛工具：扫描索尼相机 JPG+ARW 目录，对 JPG 做
**五维评分**（清晰度 / 曝光 / 噪点 / 构图 / 美学），连拍去重排序，输出
**CSV 报告**和 **XMP 星级侧车**。全程本地运行、照片不上传。

> 当前状态：**v1.1 已发布**，并已合入 M6 缺陷修复（AI 预处理通道顺序、EXIF 方向、
> 构图主体脸门槛、曝光 EV 容差带 + 主体感知、缓存配置指纹、场景预设）。
> Phase 2 批量 RAW 开发规划中，详见 `DESIGN.md` §9。

## 构建

```bash
cargo build --release
```

产物（`target/release/`，Windows 加 `.exe`）：

| 二进制 | 用途 |
|---|---|
| `pic_process` | 主命令（`scan` / `score` / `config-template`） |
| `pic_process-tune` | 调参工具：导出原始指标 CSV |
| `pic_process-gallery` | HTML 联系表生成器（缩略图 + 分数） |
| `pic_process-debug-pose` | 诊断工具：YOLOv8-pose 检测验证 |
| `pic_process-debug-scrfd` | 诊断工具：SCRFD 检测验证 |
| `pic_process-debug-exposure` | 诊断工具：全图亮度 vs 主体脸亮度 |
| `pic_process-probe` | 诊断工具：打印 ONNX 输入输出元数据 |

> `pic_process.exe` 主二进制已静态链接 onnxruntime，单文件免 DLL；
> 辅助二进制也随 release 一同构建，可按需取用。

## 模型准备（一次性）

`score` 需要 `models/` 目录下三个模型（已 gitignore）：

| 文件 | 来源 | 大小 |
|---|---|---|
| `clipiqa_model.onnx` + `.onnx.data` | [86Cao/IQA-ONNX-Models](https://huggingface.co/86Cao/IQA-ONNX-Models)（CLIP-IQA+，learned prompts 烘焙进模型） | ~153MB |
| `scrfd_10g_bnkps.onnx` | [RuteNL/SCRFD-face-detection-ONNX](https://huggingface.co/RuteNL/SCRFD-face-detection-ONNX)（InsightFace SCRFD 10g，小脸检测强） | 16.9MB |
| `yolov8n_pose.onnx` | [Xenova/yolov8n-pose](https://huggingface.co/Xenova/yolov8n-pose)（人体姿态，人脸漏检时定位头部） | 13.5MB |

模型缺失时 `score` 自动降级为纯像素评分并提示；`--no-ai` 可显式跳过。
下载后放到 `models/` 即可，无需 `download-models` 子命令。

## 用法

```bash
# 只建索引（EXIF + 配对，不评分）
pic_process scan <照片目录> -o report.csv

# 评分 + 连拍去重（推荐）
pic_process score <照片目录> -o report.csv

# 评分 + 写 XMP 星级侧车（Lightroom 可读）
pic_process score <照片目录> --xmp

# 增量重跑（SQLite 缓存，只处理新照片/变更照片）
pic_process score <照片目录>            # 第二次几乎秒级

# 其他选项
pic_process score <目录> -k 1           # 连拍子簇只保留第 1 名
pic_process score <目录> --no-ai        # 跳过 AI 推理
pic_process score <目录> --no-cache     # 禁用缓存
pic_process score <目录> --cache x.db   # 指定缓存文件
pic_process score <目录> --config x.toml # 自定义评分配置（多场景可存多份）

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

## 输出说明

**CSV**（`report.csv`）每行一张照片（ARW 分数映射自同名 JPG）：

| 列 | 含义 |
|---|---|
| `path, filename, extension, is_raw, has_pair` | 文件标识与 JPG/ARW 配对状态 |
| `date_time_original` | EXIF 拍摄时间（连拍聚类用） |
| `camera_make, camera_model, lens_model` | 相机/镜头 |
| `iso, f_number, shutter_speed, focal_length` | 曝光参数 |
| `sharpness_score, exposure_score, noise_score, composition_score, aesthetic_score` | 五维子分（0-100） |
| `total_score` | 加权总分（0-100，跨批次可比） |
| `stars` | 星级 1-5（默认按**本批次相对排名**，见下） |
| `faces` | SCRFD 检测到的人脸数 |
| `burst_group, burst_size, burst_rank, burst_keep` | 连拍去重：组号、组内张数、组内排名、是否建议保留 |

**XMP 侧车**（`--xmp`）：写 `<stem>.xmp`（如 `DSC00001.xmp`），含
`xmp:Rating`（1-5 星）+ `firstcut:` 命名空间（五维子分/人脸/连拍信息）。
同一 stem 的 JPG/ARW 共用一个侧车（分数本来就映射自 JPG）。
**已有其他软件写的侧车不会被覆盖**（只提示跳过）。

> 命名兼容性：`<stem>.xmp` 是 **Lightroom / Camera Raw** 的约定，**darktable 也读**
> 这种格式（它自己的 `<stem>.<扩展名>.xmp` 也认）。所以一份侧车两边都能用。

## 评分维度（默认权重，总和 1.0）

| 维度 | 权重 | 方法 |
|---|---|---|
| 清晰度 | 0.30 | 主体感知三层链路：SCRFD 人脸区域 reblur P80 → 人脸漏检时 YOLOv8-pose 头部关键点区域 reblur → 都无则 50 分中性下限（大光圈浅景深照片不会被误判） |
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

**连拍去重**：拍摄时间间隔 ≤2s 成组 → dHash 汉明距离 ≤10 分簇 → 簇内按总分
排序，`-k` 控制每簇保留数（默认 2），`burst_keep=true|false` 标记建议保留。

## 性能（16 核机器实测，119 张 33MP JPG）

- 冷缓存（含 AI）：约 **155ms/张**，119 张 18.4s（CLIPIQA + SCRFD + 姿态）
- 纯像素冷缓存：约 **90ms/张**（`--no-ai`）
- 增量重跑：秒级（SQLite 缓存，键 = `path` + `size` + `mtime` + `CACHE_VERSION` + 配置指纹）
- 评分参数/`--config` 变更会自动使缓存失效（配置指纹参与缓存键），无需手动换缓存文件

## 目录结构

```
src/
├── main.rs        # CLI（scan / score / config-template）
├── lib.rs         # 库入口（pub mod 各模块）
├── scan.rs        # 目录扫描 + EXIF + JPG/ARW 配对
├── decode.rs      # JPEG 解码（box 降采样）+ EXIF 方向 + 灰度/直方图
├── metrics/       # sharpness / exposure / noise / composition
├── ai/            # CLIPIQA + SCRFD + YOLOv8-pose（ort 推理）
├── dedup.rs       # 连拍分组 + dHash 聚类 + 排序
├── cache.rs       # SQLite 增量缓存（键含配置指纹）
├── output/        # csv / xmp 侧车
├── config.rs      # 权重与曲线参数 + 场景预设（TOML 可配）
└── bin/
    ├── tune.rs             # 原始指标导出（调参用）
    ├── gallery.rs          # HTML 联系表生成器
    ├── debug_pose.rs       # 诊断：YOLOv8-pose 检测
    ├── debug_scrfd.rs      # 诊断：SCRFD 检测
    ├── debug_exposure.rs   # 诊断：全图亮度 vs 主体脸亮度
    └── probe.rs            # 诊断：ONNX 模型元数据

presets/                  # 场景预设（编译进二进制，config-template --preset 输出）
├── portrait.toml  stage.toml  highkey.toml  sports.toml  lowlight.toml
```

## 测试

```bash
cargo test --lib                    # 单元测试（26 项）
cargo test --test integration_test  # 集成测试（6 项，需要 testpic/）
```

- **单元测试** 26 项（`cargo test --lib`）：
  - `dedup` 6 项（datetime 解析、闰年/平年、严格 dHash、连拍分组、dHash 距离切分、空时间无连拍）
  - `metrics::composition` 4 项（无脸中性、三分法偏好、理想大小、微小人脸降分）
  - `metrics::exposure` 7 项（sRGB↔EV 换算、容差带内满分、带外单调衰减、
    两侧容差独立、主体感知单向修正、暗背景救回、剪裁惩罚）
  - `output::xmp` 5 项（绝对阈值分档、XMP 关键字段、相对分档百分位、
    同分并列同星、absolute 模式）
  - `config` 2 项（全部场景预设可加载且参数自洽、未知预设名返回 None）
  - `scan` 2 项（配对键含目录、侧车命名保留大小写）
- **集成测试** 6 项（`tests/integration_test.rs`）：端到端 pipeline 验证（扫描/配置/dedup/总分/星级映射/模板）
  - 依赖 `testpic/` 真实照片目录（已 gitignore，私人照片不入库）
  - 跑前需先准备好照片目录；纯克隆仓库运行该集成测试会 panic（`#[ignore]` 改造见 Phase 2 TODO）

## 相关文档

- [`DESIGN.md`](DESIGN.md) — 设计文档（技术选型、评分引擎、Phase 2 规划）
- [`release_notes.md`](release_notes.md) — 版本说明
- [`M5_REVIEW.md`](M5_REVIEW.md) — M5 调参决策历史（已落地，存档备查）

## Phase 2（规划中，未实现）

批量 RAW 开发：darktable-cli 引擎 + lensfun 镜头校正 + neural restore AI 降噪，
详见 `DESIGN.md` §9。
