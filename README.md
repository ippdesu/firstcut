# firstcut — 索尼照片初筛评分工具

用 Rust 编写的本地照片初筛工具：扫描索尼相机 JPG+ARW 目录，对 JPG 做
**五维评分**（清晰度 / 曝光 / 噪点 / 构图 / 美学），连拍去重排序，输出
**CSV 报告**和 **Lightroom 兼容的 XMP 星级侧车**。全程本地运行、照片不上传。

> 当前状态：**v1.0 已发布**（Phase 1 完整交付：M0 扫描 → M1 像素指标 → M2 连拍去重
> → M3 AI 评分 → M4 XMP+缓存 → M5 调参验证）。Phase 2 批量 RAW 开发规划中，详见 `DESIGN.md` §9。

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

# 生成评分配置模板（人像/打鸟/夜景各存一份）
pic_process config-template -o portrait.toml

# 辅助工具
pic_process-gallery report.csv -o gallery.html   # HTML 联系表（缩略图+分数）
pic_process-tune <目录> -o metrics.csv           # 原始指标（调参用）
```

## 多场景配置

不同拍摄场景用不同权重：人像（构图/美学权重高）、打鸟（清晰度权重高）、
夜景（曝光权重低 + 曝光目标亮度调低）。`config-template` 生成模板后按需修改，
`--config` 加载；**换配置时建议换缓存文件名**（`--cache night.sqlite`）。

## 输出说明

**CSV**（`report.csv`）每行一张照片（ARW 分数映射自同名 JPG）：

| 列 | 含义 |
|---|---|
| `path, filename, extension, is_raw, has_pair` | 文件标识与 JPG/ARW 配对状态 |
| `date_time_original` | EXIF 拍摄时间（连拍聚类用） |
| `camera_make, camera_model, lens_model` | 相机/镜头 |
| `iso, f_number, shutter_speed, focal_length` | 曝光参数 |
| `sharpness_score, exposure_score, noise_score, composition_score, aesthetic_score` | 五维子分（0-100） |
| `total_score` | 加权总分（0-100） |
| `faces` | SCRFD 检测到的人脸数 |
| `burst_group, burst_size, burst_rank, burst_keep` | 连拍去重：组号、组内张数、组内排名、是否建议保留 |

**XMP 侧车**（`--xmp`）：按 Lightroom 命名约定写 `<stem>.<原扩展名>.xmp`，
含 `xmp:Rating`（1-5 星）+ `firstcut:` 命名空间（五维子分/人脸/连拍信息）。
**已有其他软件写的侧车不会被覆盖**（只提示跳过）。

## 评分维度（默认权重，总和 1.0）

| 维度 | 权重 | 方法 |
|---|---|---|
| 清晰度 | 0.30 | 主体感知三层链路：SCRFD 人脸区域 reblur P80 → 人脸漏检时 YOLOv8-pose 头部关键点区域 reblur → 都无则 50 分中性下限（大光圈浅景深照片不会被误判） |
| 曝光 | 0.25 | 过曝/欠曝像素比例（4× 惩罚）+ 平均亮度偏离 `exposure_target`（默认 128，可配）的高斯衰减 |
| 噪点 | 0.15 | 暗部 8×8 块标准差 P15（最平滑暗块）+ ISO 容忍度曲线 `k = 3.0·(1+0.3·log10(iso/100))` |
| 构图 | 0.15 | SCRFD 人脸（无人脸时 YOLOv8-pose 人体框）：三分法位置 + 主体大小（2~30% 理想）+ 多人降权；无主体中性 60 |
| 美学 | 0.15 | CLIPIQA+（CLIP 底座，sigmoid 输出 ×100 → 0-100 分） |

> 加载 `--config` 时，权重和需在 1.0±0.05 范围内；总和偏差超 5% 会被拒绝加载。

**星级分档**（默认，可配置）：≥75→5★ / ≥60→4★ / ≥45→3★ / ≥30→2★ / 其余 1★

**连拍去重**：拍摄时间间隔 ≤2s 成组 → dHash 汉明距离 ≤10 分簇 → 簇内按总分
排序，`-k` 控制每簇保留数（默认 2），`burst_keep=true|false` 标记建议保留。

## 性能（16 核机器实测，119 张 33MP JPG）

- 冷缓存（含 AI）：约 **155ms/张**，119 张 18.4s（CLIPIQA + SCRFD + 姿态）
- 纯像素冷缓存：约 **90ms/张**（`--no-ai`）
- 增量重跑：秒级（SQLite 缓存，键 = `path` + `size` + `mtime` + `CACHE_VERSION`）
- 评分参数变更自动使缓存失效（`CACHE_VERSION` 提升）；换 --config 建议换 --cache 文件名

## 目录结构

```
src/
├── main.rs        # CLI（scan / score / config-template）
├── lib.rs         # 库入口（pub mod 各模块）
├── scan.rs        # 目录扫描 + EXIF + JPG/ARW 配对
├── decode.rs      # JPEG 解码（box 降采样）+ 灰度/直方图
├── metrics/       # sharpness / exposure / noise / composition
├── ai/            # CLIPIQA + SCRFD + YOLOv8-pose（ort 推理）
├── dedup.rs       # 连拍分组 + dHash 聚类 + 排序
├── cache.rs       # SQLite 增量缓存
├── output/        # csv / xmp 侧车
├── config.rs      # 权重与曲线参数（TOML 可配）
└── bin/
    ├── tune.rs          # 原始指标导出（调参用）
    ├── gallery.rs       # HTML 联系表生成器
    ├── debug_pose.rs    # 诊断：YOLOv8-pose 检测
    ├── debug_scrfd.rs   # 诊断：SCRFD 检测
    └── probe.rs         # 诊断：ONNX 模型元数据
```

## 测试

```bash
cargo test --lib           # 单元测试（12 项）
cargo test --test integration_test  # 集成测试（5 项，需要 testpic/）
```

- **单元测试** 12 项（`cargo test --lib`）：
  - `dedup` 6 项（datetime 解析、闰年/平年、严格 dHash、连拍分组、dHash 距离切分、空时间无连拍）
  - `metrics::composition` 4 项（无脸中性、三分法偏好、理想大小、微小人脸降分）
  - `output::xmp` 2 项（星级分档边界、XMP 关键字段）
- **集成测试** 5 项（`tests/integration_test.rs`）：端到端 pipeline 验证（扫描/配置/dedup/总分/星级映射/模板）
  - 依赖 `testpic/` 真实照片目录（已 gitignore，私人照片不入库）
  - 跑前需先准备好照片目录；纯克隆仓库运行该集成测试会 panic（`#[ignore]` 改造见 Phase 2 TODO）

## 相关文档

- [`DESIGN.md`](DESIGN.md) — 设计文档（技术选型、评分引擎、Phase 2 规划）
- [`release_notes.md`](release_notes.md) — 版本说明
- [`M5_REVIEW.md`](M5_REVIEW.md) — M5 调参决策历史（已落地，存档备查）

## Phase 2（规划中，未实现）

批量 RAW 开发：darktable-cli 引擎 + lensfun 镜头校正 + neural restore AI 降噪，
详见 `DESIGN.md` §9。
