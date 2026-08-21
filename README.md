# firstcut — 索尼照片初筛评分工具

用 Rust 编写的本地照片初筛工具：扫描索尼相机 JPG+ARW 目录，对 JPG 做
**五维评分**（清晰度 / 曝光 / 噪点 / 构图 / 美学），连拍去重排序，输出
**CSV 报告**和 **Lightroom 兼容的 XMP 星级侧车**。全程本地运行、照片不上传。

## 构建

```bash
cargo build --release
```

产物：`target/release/pic_process.exe`（onnxruntime 已静态链接，单文件免 DLL）。

## 模型准备（一次性）

`score` 需要 models/ 目录下三个文件（已 gitignore）：

| 文件 | 来源 | 大小 |
|---|---|---|
| `musiq_model.onnx` + `.onnx.data` | [86Cao/IQA-ONNX-Models](https://huggingface.co/86Cao/IQA-ONNX-Models)（国内可用 hf-mirror.com） | ~110MB |
| `scrfd_10g_bnkps.onnx` | [RuteNL/SCRFD-face-detection-ONNX](https://huggingface.co/RuteNL/SCRFD-face-detection-ONNX)（InsightFace SCRFD 10g，小脸检测强） | 16.9MB |

模型缺失时 score 自动降级为纯像素评分并提示；`--no-ai` 可显式跳过。

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

# 辅助工具
pic_process-gallery report.csv -o gallery.html   # HTML 联系表（缩略图+分数）
pic_process-tune <目录> -o metrics.csv           # 原始指标（调参用）
```

## 输出说明

**CSV**（report.csv）每行一张照片（ARW 分数映射自同名 JPG）：
`path, filename, extension, is_raw, has_pair, date_time_original, 相机信息,
ISO/光圈/快门/焦距, sharpness_score, exposure_score, noise_score,
composition_score, aesthetic_score, total_score, faces,
burst_group, burst_size, burst_rank, burst_keep`

**XMP 侧车**（`--xmp`）：按 Lightroom 命名约定写 `<名>.<原扩展名>.xmp`，
含 `xmp:Rating`（1-5 星）+ `firstcut:` 命名空间（五维子分/人脸/连拍信息）。
**已有其他软件写的侧车不会被覆盖**（只提示跳过）。

## 评分维度（默认权重，总和 1.0）

| 维度 | 权重 | 方法 |
|---|---|---|
| 清晰度 | 0.35 | 主体感知：检出人脸时用人脸区域 reblur 差分 P80（合焦边缘证据），否则全局梯度（Sobel 方差/亮度方差）并给 50 分中性下限——大光圈浅景深照片不会被误判 |
| 曝光 | 0.20 | 过曝/欠曝像素比例 + 平均亮度偏离中间调 |
| 噪点 | 0.15 | 暗部 8×8 块标准差 P15（最平滑暗块）+ ISO 容忍度曲线 |
| 构图 | 0.15 | SCRFD 人脸检测：三分法位置 + 人脸大小 + 多人降权；无人脸中性 60 |
| 美学 | 0.15 | MUSIQ 0-100 质量分 |

星级分档：≥80→5★ / ≥65→4★ / ≥50→3★ / ≥35→2★ / 其余 1★

连拍去重：拍摄时间间隔 ≤2s 成组 → dHash 汉明距离 ≤10 分簇 → 簇内按总分
排序，`-k` 控制每簇保留数（默认 2），`burst_keep` 标记建议保留。

## 性能（16 核机器实测）

- 冷缓存：33MP JPG 约 **90ms/张**（解码 + 五维评分），119 张 10.6s
- 增量重跑：秒级（SQLite 缓存，键 = 文件大小 + mtime + 分析版本）
- 评分参数变更自动使缓存失效（CACHE_VERSION）

## 目录结构

```
src/
├── main.rs        # CLI（scan / score）
├── scan.rs        # 目录扫描 + EXIF + JPG/ARW 配对
├── decode.rs      # JPEG 解码（box 降采样）+ 灰度/直方图
├── metrics/       # sharpness / exposure / noise / composition
├── ai/            # musiq / facedetect（ort 推理）
├── dedup.rs       # 连拍分组 + dHash 聚类 + 排序
├── cache.rs       # SQLite 增量缓存
├── output/        # csv / xmp 侧车
├── config.rs      # 权重与曲线参数
└── bin/           # tune（调参）、gallery（联系表）
```

## Phase 2（规划中，未实现）

批量 RAW 开发：darktable-cli 引擎 + lensfun 镜头校正 + neural restore AI 降噪，
详见 `DESIGN.md` 第 9 节。
