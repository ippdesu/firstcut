# firstcut — 索尼照片本地初筛与复核工具

用 Rust 编写的本地照片初筛评分与复核工具：扫描索尼相机 `JPG + ARW` 目录，对 JPG 做**五维评分**（清晰度 / 曝光 / 噪点 / 构图 / 美学）与**连拍姿态去重**，提供本地 Web 复核界面，并输出 **CSV 报告** 与 **Lightroom XMP 星级侧车**。全程本地运行，照片不上传、不删改原片。

> **当前版本**：`v1.4.2`（提供 Windows 可执行程序与 Linux `amd64` / `arm64` 容器镜像）
> **当前边界**：聚焦本地初筛、人工复核与 Lightroom 元数据交接；图像分析支持 SDR JPEG；darktable 批量 RAW 开发路线已停止（见 [历史研究档案](docs/history/DARKTABLE_PHASE2_RESEARCH.md)）。

## 文档导航

| 文档 | 内容 |
|---|---|
| [设计文档](docs/DESIGN.md) | 现行架构、四层数据模型、评分/缓存/配置约定、安全边界与后续计划 |
| [版本说明](docs/RELEASE_NOTES.md) | 面向用户的各版本变更记录 |
| [安全政策](SECURITY.md) | 安全支持版本与私密漏洞报告渠道 |
| [文档总索引](docs/README.md) | 现行文档与历史归档维护规范 |
| [历史与过期评审归档](docs/history/README.md) | 历次代码评审、早期里程碑、历史决策与已停止的 darktable 研究 |

---

## 核心架构概览

```text
扫描 / EXIF / JPG+ARW 配对 (scan)
        ↓  [cap-std 安全目录句柄边界: storage]
增量分析缓存匹配 (cache.sqlite, CACHE_VERSION = 15)
        ↓  未命中时
JPEG 解码 + 像素指标 (decode / metrics) + ONNX AI 推理 (ai: SCRFD / YOLOv8-pose / CLIPIQA+)
        ↓
统一结果合成 (selection / dedup / decision: 当前权重、相对星级、连拍簇、人工决定覆盖)
        ↓
CSV 报告 / XMP 侧车 (output)  &  本地 Web 复核台 (review: axum + 内嵌前端)
```

- **四层数据分离**：原始照片（只读资产）→ 可重建分析缓存（`.firstcut/cache.sqlite`）→ 运行期算法建议（`selection` 共用逻辑）→ 持久化人工决定（`.firstcut/decisions.sqlite`，重评分不覆盖）。
- **UI 与 CLI 统一**：同一照片根目录共用 `.firstcut/` 下的缓存、配置、报告与 RAW 关联偏好，避免切换入口导致数据不一致。
- 详细架构约定、算法细节与安全设计见 [`docs/DESIGN.md`](docs/DESIGN.md)。

---

## 快速开始

### 方式一：Windows 双击启动复核 UI（推荐）

从 [GitHub Releases](https://github.com/ippdesu/firstcut/releases) 下载发行包，双击 `firstcut-ui.exe`：

1. **选择照片根目录**：
   - 若 JPG 与 RAW 分放在 `JPG/`、`RAW/` 子目录，请选择同时包含两者的**活动根目录**；仅有 JPG 时可直接选 JPG 目录。
   - 默认递归扫描所选目录，并跳过名为 `Processed/` 的输出子目录（要浏览成片可直接选择 `Processed/` 本身；建议不要把成片散放在活动根目录顶层）。
2. **模型目录**：发行包不含 AI 模型（见下方[模型准备](#模型准备一次性)）。请将模型放到 `firstcut-ui.exe` 同级的 `models/` 目录（开发环境下启动器也会自动向上查找仓库根目录的 `models/`）。
3. **浏览与操作**：
   - 浏览器自动打开并登录，立即展示扫描阶段与进度；未找到已有评分时，可重选目录或先以未评分状态浏览，再通过侧栏**“重新评分”**触发跑批。
   - 侧栏支持切换照片目录与**“关联同名 RAW 照片”**开关（偏好保存在 `.firstcut/ui-preferences.json`，UI 与 CLI 共用）。
   - 页面右上角提供**“重启服务”**（重载配置与快照）和**“退出程序”**（释放 `8787` 端口）；评分运行中会拒绝这两项操作。仅关闭浏览器标签页不会退出服务，再次双击 `firstcut-ui.exe` 可重新打开运行中的页面。

### 方式二：命令行（CLI）

```bash
# 1. 仅扫描建索引（EXIF + JPG/ARW 配对，不评分）
pic_process scan <照片目录>                  # 默认写入 <照片目录>/.firstcut/scan-report.csv

# 2. 五维评分 + 连拍去重（增量缓存，第二次运行几乎秒级）
pic_process score <照片目录>                 # 默认写入 <照片目录>/.firstcut/report.csv 与 cache.sqlite

# 3. 评分并导出 Lightroom XMP 星级侧车
pic_process score <照片目录> --xmp

# 4. 启动本地 Web 复核界面（默认监听 http://127.0.0.1:8787，终端 Ctrl+C 退出）
pic_process review <照片目录>
pic_process review <照片目录> --port 9000 --config stage.toml

# 5. 生成评分配置模板或场景预设（portrait / stage / highkey / sports / lowlight）
pic_process config-template
pic_process config-template --preset stage -o <照片目录>/.firstcut/config.toml

# 6. 辅助工具
pic_process-gallery <照片目录>/.firstcut/report.csv   # 生成静态 HTML 联系表（默认输出到报告同目录）
pic_process-tune <照片目录>                            # 导出原始指标（默认 <照片目录>/.firstcut/metrics.csv）
```

常用 `score` 选项：
- `--xmp`：写出 `<stem>.xmp` 侧车（默认仅写 `xmp:Rating`、`firstcut:*` 与信息字段 `firstcut:suggestedEV`，不触碰 Adobe `crs:*` 开发参数）
- `--xmp-exposure`：配合 `--xmp` 显式将建议曝光写入 Adobe `crs:Exposure2012`（亦可在配置中设 `[metric] exposure_write_crs = true`）；绝不覆盖或删除侧车中已有的 Lightroom 人工曝光
- `-k <N>`：每个连拍保留单元（姿态簇 / dHash 子簇）保留前 N 张（默认读取 `[dedup] keep_k = 3`）
- `--config <file>`：指定评分配置 TOML（默认自动读取 `<照片目录>/.firstcut/config.toml`）
- `--cache <file>` / `--no-cache`：指定旧版/自定义缓存路径，或禁用缓存
- `--no-ai`：跳过 AI 推理，仅跑纯像素模式
- `--raw-csv`：输出不做表格公式转义的原始 CSV（仅限脚本读取，勿直接用电子表格打开不可信来源）
- `--gpu`：实验性 DirectML GPU 推理（需 `--features gpu` 构建；历史实测无显著加速）

> **访问凭据与链接**：复核链接自带自动登录凭据；使用 `--no-browser` 时请复制终端输出的完整链接（勿分享含凭据的链接）。直接访问不带凭据的地址需手动输入凭据或复用已有会话。

### 方式三：Linux / macOS（Docker 容器）

Release 工作流向 GHCR 发布 `linux/amd64` 与 `linux/arm64` 双架构镜像（Apple Silicon M1–M4 可通过 Docker Desktop 原生运行 ARM64 Linux 镜像；原生 macOS 二进制尚未列入发行承诺）。

> 首次发布到新仓库时，需在 GitHub 仓库关联的 `firstcut` package 设置中将可见性改为 **Public**，他人才能匿名拉取。

1. 复制 `.env.example` 为 `.env`，生成随机访问凭据（`openssl rand -hex 32`）并填写路径：

```dotenv
PHOTO_DIR=/Users/你的用户名/Pictures/活动目录
MODEL_DIR=/Users/你的用户名/firstcut-models
FIRSTCUT_ACCESS_TOKEN=<随机生成的 32-128 位凭据>
```

2. 映射当前用户权限并启动容器：

```bash
export FIRSTCUT_UID="$(id -u)"
export FIRSTCUT_GID="$(id -g)"
docker compose pull
docker compose up -d
```

3. 打开 <http://127.0.0.1:8787> 输入 `FIRSTCUT_ACCESS_TOKEN` 登录；使用完毕后运行 `docker compose down`。

- 容器默认只映射宿主机回环地址 `127.0.0.1:8787`，`PHOTO_DIR` 以读写方式挂载以保存 `.firstcut/` 状态与 XMP，`MODEL_DIR` 只读挂载到 `/app/models`。
- 直接运行 CLI 并使用 `--bind 0.0.0.0` 时，必须提供 `FIRSTCUT_ACCESS_TOKEN`；若经端口映射或反向代理暴露，请配置 `FIRSTCUT_PUBLIC_URL`（非本机地址须使用 HTTPS，且代理需保留外部 `Host` 与 `Origin`）。

---

## 模型准备（一次性）

`score` 完整模式需要 `models/` 目录（或 `FIRSTCUT_MODEL_DIR` 指定目录）下的三个固定版本模型（已 gitignore，不随源码或 Release 分发）：

| 文件 | 来源 | 大小 |
|---|---|---|
| `clipiqa_model.onnx` + `clipiqa_model.onnx.data` | [86Cao/IQA-ONNX-Models 固定版本](https://huggingface.co/86Cao/IQA-ONNX-Models/tree/2e32ab1ad40c592da9d199f4c7fb8a2383bbae94)（保留原文件名） | ~153 MB |
| `scrfd_10g_bnkps.onnx` | [RuteNL 的 10g_bnkps.onnx](https://huggingface.co/RuteNL/SCRFD-face-detection-ONNX/blob/3d9a1b3bc9f8a50635817929118fb9184f5bc30b/10g_bnkps.onnx)（下载后改成左侧文件名） | 16.9 MB |
| `yolov8n_pose.onnx` | [Xenova 的 onnx/model.onnx](https://huggingface.co/Xenova/yolov8n-pose/blob/da4224085e2f6ce4c9ff9b670e28765194619db2/onnx/model.onnx)（下载后改成左侧文件名） | 13.5 MB |

推理前会核对上述固定版本的 **SHA‑256**。模型缺失或校验失败时，`score` 会提示并降级为纯像素评分；CSV 的 `analysis_mode` 列会标明 `ai` 或 `pixel`，两者在缓存中严格隔离。

---

## 评分、预设与连拍去重

### 五维评分（默认权重总和 1.0）

| 维度 | 默认权重 | 核心方法 |
|---|---|---|
| 清晰度 | 0.30 | 主体感知三层链路：SCRFD 人脸区域 reblur P80 → 无主体脸时 YOLOv8-pose 头部区域 → 无主体则 50 中性下限；与全局 Tenengrad 取高者 |
| 曝光 | 0.25 | 过曝/欠曝像素比例惩罚 + **EV 容差带**（默认 ±1 EV 满分，-4 / +2 EV 归零）；有主体脸时做向中灰方向单向修正 |
| 噪点 | 0.15 | 暗部 8×8 块标准差 P15 + ISO 容忍度曲线 `k = 3.0·(1 + 0.3·log10(iso/100))` |
| 构图 | 0.15 | SCRFD 主体脸（高度 ≥ 4%）三分法位置、占比（8%–30% 理想）与多人降权；无人脸给中性 60 |
| 美学 | 0.15 | CLIPIQA+（224×224，sigmoid × 100） |

### 内置场景预设

通过 `pic_process config-template --preset <名称>` 生成，或在复核界面直接编辑 `.firstcut/config.toml`：

| 预设 | 适用场景 | 主要调整 |
|---|---|---|
| `portrait` | 人像 / 漫展 / 棚拍 | 等同内置默认值 |
| `stage` | 舞台演出 / 暗厅 / live | 曝光权重 0.20、暗侧容差放宽至 -2 EV、构图权重 0.20 |
| `highkey` | 白背景 / 白裙 / 雪景 | 目标亮度 150、亮侧容差放宽至 +3 EV |
| `sports` | 运动 / 打鸟 / 飞机 | 清晰度权重 0.45 且更严格、构图权重 0.05、关闭主体感知曝光 |
| `lowlight` | 夜景 / 暗光室内 | 目标亮度 110、暗侧容差 -5 EV、噪点更宽容、美学权重 0.20 |

> **配置校验与缓存规则**：配置采用 **fail-fast** 校验（文件缺失、未知字段、权重和偏离 `1.0 ± 0.05`、NaN/越界或容差带顺序错误均直接报错）。修改**权重/星级阈值/连拍参数**不会触发重解码和 AI 重算，仅重新合成总分与星级；修改**曲线参数**、分析模式或模型来源时自动失效并重算。

### 星级分档与连拍去重

- **相对星级（默认 `star_mode = "relative"`）**：按批次内排名给星——5★ 前 10%、4★ 10%–30%、3★ 30%–65%、2★ 65%–90%、1★ 后 10%（同分并列取平均位次；建议整场活动一次评分，跨批次比较请看 `total_score`）。可切 `star_mode = "absolute"`（默认阈值 75/60/45/30）。
- **人工星级优先**：通过复核界面设置的星级持久化在 `.firstcut/decisions.sqlite`，重评分不会覆盖，并同步给配对的 JPG/ARW。
- **自适应连拍去重**：按 EXIF 时间 ≤ 2s 分连拍组 → dHash 汉明距离 ≤ 10 分近似子簇 → SCRFD 五关键点 10 维姿态描述子聚类（阈值 0.25）→ 每姿势簇保留 top-3（单组上限 20）。可在 `[dedup]` 关闭 `adaptive_keep` 回退纯 dHash 模式。`burst_keep` 仅为建议标记，工具永不删除文件。

---

## 本地复核界面（`review`）

- **缩略图墙与筛选侧栏**：分页懒加载、筛选排序自动记忆；默认按连拍组折叠显示代表帧，点击或右键可展开整组、手动覆盖保留/舍弃标记。侧栏筛选区可折叠并拖动调宽。
- **1:1 原图灯箱与连拍并排对比**：按需流式直读原图，支持滚轮缩放、拖拽平移、双击复位、`←/→` 组内切换、缩放百分比与带单位 EXIF（`f/`、`s`、`mm`）；支持勾选同组 2–4 张**同步缩放平移**对比合焦。
- **RGBW 直方图**：可拖动悬浮面板，支持 W/R/G/B 通道独立开关，基于解码后 SDR JPEG 做网格抽样（默认 10%，可在设置中调为 1%–25%，仅内存缓存）。
- **评分解释与场景反馈**：展示活动照片的五维子分、实际权重与贡献；支持记录真实场景纠正与备注至 `.firstcut/scene-feedback.jsonl`。
- **界面内改星、跑批与调参**：灯箱内 `1–5` 快捷改星并同步 XMP；侧栏一键重评分（实时进度与时间日志）；表单化编辑配置并用 `toml_edit` 保留注释。

---

## 输出与状态文件布局

除照片旁的 `<stem>.xmp` 侧车（供 Lightroom / Camera Raw / darktable 读取）外，所有 firstcut 状态默认集中存放在 `<照片根目录>/.firstcut/`：

| 路径 | 说明 |
|---|---|
| `.firstcut/cache.sqlite` | 可重建的分析缓存（删除后可重算） |
| `.firstcut/decisions.sqlite` | 人工星级决定库（**备份照片时请保留**） |
| `.firstcut/burst-overrides.json` | review 人工连拍保留/舍弃覆盖（**建议保留**） |
| `.firstcut/scene-feedback.jsonl` | 场景标注与备注记录（上限 16 MiB） |
| `.firstcut/config.toml` / `ui-preferences.json` | 照片根级评分配置与 UI 偏好 |
| `.firstcut/report.csv` / `scan-report.csv` / `metrics.csv` | 评分、扫描与调参报告 |
| `.firstcut/thumbs/v2/` | 有界缩略图缓存（8192 槽，每槽上限 64 KiB） |
| `.firstcut/review.log` | 任务时间日志（严格限制单文件 256 KiB） |
| `<照片目录>/<stem>.xmp` | 默认写 `xmp:Rating`、`firstcut:*` 子分/连拍与 `firstcut:suggestedEV`；仅显式开启 `--xmp-exposure` / `exposure_write_crs = true` 或 UI 对应选项时，才为尚无曝光参数的侧车附加 `crs:Exposure2012`（含 `crs:HasSettings`），改星仅同步 `xmp:Rating`；不覆盖他人侧车，绝不覆盖或删除已有 Adobe 人工曝光，并兼容属性规范化写法 |

> **安全边界**：程序拒绝通过符号链接或 Windows 重解析点读写相册状态，并拒绝读取带未关闭旁路日志的异常 SQLite 数据库（保留原件不破坏）。正常关闭的旧版评分库和决定库可直接继续使用。

<details>
<summary>查看 CSV 报告完整列说明</summary>

| 列 | 含义 |
|---|---|
| `path, filename, extension, is_raw, has_pair` | 文件路径标识与 JPG/ARW 配对状态 |
| `date_time_original` | EXIF 拍摄时间（连拍聚类依据） |
| `camera_make, camera_model, lens_model` | 相机与镜头型号 |
| `iso, f_number, shutter_speed, focal_length` | 曝光参数 |
| `sharpness_score, exposure_score, noise_score, composition_score, aesthetic_score` | 五维子分（0–100） |
| `total_score` | 加权总分（0–100，同配置下跨批次可比） |
| `stars` | 最终星级 1–5（人工决定优先） |
| `rating_source` | `algorithm`（算法建议）或 `manual`（人工星级） |
| `analysis_mode` | `ai`（完整模型）或 `pixel`（纯像素/降级） |
| `faces` | SCRFD 检测到的人脸数 |
| `analysis_ok` | 分析数据是否可用（解码失败或无配对 ARW 为 `false`） |
| `burst_group, burst_size, burst_rank, burst_keep, burst_pose` | 连拍组号、保留单元张数、单元内排名、建议保留标记、姿态簇号 |

</details>

---

## 源码构建、目录结构与测试

### 从源码构建

```bash
cargo build --release
# 可选：实验性 DirectML GPU 推理（score --gpu）
cargo build --release --features gpu
```

产物位于 `target/release/`（Windows 加 `.exe`；ONNX Runtime 静态链接，Windows 依赖系统 DirectML 与 Visual C++ 运行时）：

| 二进制 | 用途 | 是否进入 Release |
|---|---|---|
| `pic_process` | 主命令行工具（`scan` / `score` / `config-template` / `review`） | 是 |
| `firstcut-ui` | Windows 无控制台双击启动器 | 是 |
| `pic_process-gallery` | 静态 HTML 联系表生成器 | 是 |
| `pic_process-tune` | 原始指标 CSV 导出（调参用） | 是 |
| `pic_process-debug-pose` / `-scrfd` / `-exposure` / `-probe` | 模型与像素指标诊断工具 | 按需源码构建 |

### 源码目录结构

```text
src/
├── main.rs            # CLI 入口（scan / score / config-template / review）
├── lib.rs             # 库模块导出
├── storage.rs         # 基于 cap-std 的安全目录/文件访问与防链接逃逸边界
├── scan.rs            # 目录扫描、EXIF 解析与 JPG/ARW 配对
├── decode.rs          # JPEG 快速解码（box 降采样）、EXIF 方向与灰度安全路径
├── score.rs           # 并行评分管线与 CLI/UI 共用跑批入口
├── config.rs          # 配置加载、严格校验、指纹计算与内置场景预设
├── cache.rs           # SQLite 增量分析缓存（CACHE_VERSION = 15）
├── dedup.rs           # 连拍时间分组、dHash 子簇与 SCRFD 姿态聚类
├── decision.rs        # 独立人工星级存储与最终星级合并
├── selection.rs       # CLI / review 共用的总分、星级与连拍结果合成
├── metrics/           # sharpness / exposure / noise / composition
├── ai/                # CLIPIQA+ / SCRFD / YOLOv8-pose 推理与 SHA-256 校验
├── output/            # CSV（公式防护）与 XMP 侧车读写合并
├── review/            # 本地 Web 复核服务（axum）
│   ├── mod.rs         # HTTP 路由与服务生命周期
│   ├── access.rs      # 访问凭据兑换、会话与 Host/Origin 校验中间件
│   ├── snapshot.rs    # 快照构建与加载状态
│   ├── job.rs         # 后台跑批任务与 256 KiB 有界日志
│   ├── config_edit.rs # 保留注释的 TOML 配置编辑
│   ├── scene_feedback.rs # 场景反馈追加与大小上限保护
│   ├── thumb.rs       # 固定槽位缩略图缓存
│   └── assets/        # 内嵌前端（index.html + review.js）
└── bin/               # firstcut_ui / gallery / tune / debug_* / probe

presets/               # 5 份内置场景预设 TOML（编译进二进制）
docs/                  # 现行设计、版本说明与 history/ 历史归档
```

### 测试与 CI/Release 验证

```bash
cargo test --locked --all-targets
node --check src/review/assets/review.js

# 验证已构建的 Windows 发行程序（登录、原图、缩略图、重评分与落盘）
python .github/scripts/check-review.py --binary target/release/pic_process.exe
```

- **CI（[`.github/workflows/ci.yml`](.github/workflows/ci.yml)）**：在 Windows、Linux x86_64 与 ARM64 上运行测试、RustSec 依赖审计（`cargo audit`）、合成照片复核冒烟测试与双架构容器 Trivy 静态扫描；并禁止私人照片与本地状态进入 Git。
- **Release（[`.github/workflows/release.yml`](.github/workflows/release.yml)）**：要求版本标签指向 `main` 上已通过 CI 与 CodeQL 的提交，核对 `Cargo.toml` 版本，发布带 GitHub 构建证明的 Windows 产物与双架构 GHCR 镜像。

### 历史性能参考（16 核机器实测）

- 119 张 33MP 冷缓存（含 AI）：约 **146 ms/张**；纯像素（`--no-ai`）约 **90 ms/张**。
- 1,447 张真实图库（88% 连拍）：冷跑 **2m55s**（~121 ms/张），增量全命中 **2.06s**。
- 实验性 `--gpu`（DirectML）：119 张 16.7s vs CPU 17.4s，无显著收益。

> 上述数据为历史样本与硬件实测参考，非跨设备保证；直方图抽样开销估算见 [`docs/DESIGN.md`](docs/DESIGN.md#71-直方图与性能边界)。

---

## 后续方向

当前优先推进：
1. 将人工连拍保留覆盖纳入统一决定模型与 `selection` 导出（消除 review 与 CLI/CSV 之间的一致性缺口）。
2. 建立真实选片标注集，评估好片排除率与连拍最佳帧覆盖率。
3. 完成 Lightroom / Camera Raw 实际导入验证（星级、JPG/ARW 配对与 `crs:Exposure2012` 效果）。

完整路线与未验收项见 [`docs/DESIGN.md`](docs/DESIGN.md#9-后续工作与未验收项)。
