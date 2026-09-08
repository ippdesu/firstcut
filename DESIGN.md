# 照片初筛评分工具（Rust）— 设计与实现文档

> 状态：**Phase 1 已完成并发布 v1.1**（GitHub: ippdesu/firstcut，tag v1.0 / v1.1）；M6 缺陷修复已合并 main（未另发 Release）
> 日期：2026 规划稿 / 2026 实施完成
> 需求来源：索尼相机 JPG+ARW 连拍/风景/人像选片地狱，需要自动初步评分
> 配套文档：`README.md`（用户手册）/ `release_notes.md`（版本说明）/ `M5_REVIEW.md`（M5 决策记录）/ `firstcut.toml`（配置模板）

## 0. 目标（根据确认的需求）

一个 **Rust 编写的本地 CLI 工具**：扫描索尼相机的 JPG+ARW 照片目录 → 对 JPG 做 **5 维评分（清晰度/曝光/噪点/构图/美学）+ 连拍去重** → 把分数映射到同名 ARW → 输出 **XMP 星级 + CSV 报告**。目标吞吐：上万张照片可接受的处理时间（分钟级），增量重跑秒级（SQLite 缓存）。

约束：全程本地、离线运行（仅首次下载 ONNX 模型）；CLI 优先，GUI 后续规划。

## 1. 开源项目参考（已调研）

| 项目 | 参考点 |
|---|---|
| [pixcull](https://github.com/ChrisChen667788/pixcull) | 6 轴评分维度划分、XMP/IPTC 导出、近重复分组 —— 维度设计与输出格式直接参考 |
| [facet](https://github.com/ncoevoet/facet) | "传统指标 + AI 美学"混合打分架构 |
| [best-photo-picker](https://github.com/Arkalogy/best-photo-picker) | 质量评分 + 感知去重组合 |
| [RAWviewer](https://github.com/markyip/RAWviewer) | RAW 选片 + 星级 + XMP 的本地工作流 |
| [digiKam Image Quality Sorter](https://docs.digikam.org/es/_sources/maintenance_tools/maintenance_quality.rst.txt) | 纯算法基线：清晰度/噪点/曝光的经典实现 |
| [NIMA](https://github.com/bencoster/Neural-IMage-Assessment) | AI 美学模型（PyTorch 权重，需转 ONNX） |

## 2. 技术选型（Rust 全链路，已实现）

| 用途 | Crate/方案 | 说明（实际实现） |
|---|---|---|
| CLI | `clap` | 子命令：`scan`（建索引）/ `score`（分析+评分）/ `config-template`（生成配置模板）。**`report` / `download-models` 已规划但未实现**——CSV 由 `score` 直接产出；模型缺失时给出下载链接并自动降级。 |
| JPG 解码 | `jpeg-decoder`（快速路径）+ `image` 兜底 | JPEG 全解码后 **box 块平均降采样**到 ~1MP 分析尺寸（~50ms/张 33MP；jpeg-decoder 0.3 无 DCT 缩放，故全解码+块平均） |
| EXIF | `kamadak-exif` | ISO、光圈、快门、拍摄时间（连拍聚类用）；多值 ASCII 字段取首个非空值 |
| 像素处理 | `image` / 自写 | 灰度（BT.601 加权）、直方图、Sobel 梯度、3×3 box blur（未用 `imageproc`） |
| AI 推理 | `ort` 2.0.0-rc.13（onnxruntime-rs，静态链接自包含，无需 DLL） | 跑 CLIPIQA + SCRFD + YOLOv8-pose；GPU（DirectML）暂未启用，CPU 池化收益不显著 |
| 美学评分 | **CLIPIQA+ ONNX**（[86Cao/IQA-ONNX-Models](https://huggingface.co/86Cao/IQA-ONNX-Models)） | 224×224、CLIP 归一化、`(x/255 - mean) / std`、sigmoid 输出 ×100 → 0-100 分；M5 决策从 MUSIQ 换到 CLIPIQA（区分度更高） |
| 人脸检测 | **SCRFD 10g**（[RuteNL/SCRFD-face-detection-ONNX](https://huggingface.co/RuteNL/SCRFD-face-detection-ONNX)） | 640×640 输入、(x-127.5)/128 归一化、9 个输出张量（score/bbox × stride [8,16,32]、score 已 sigmoid）；阈值 0.3 + 贪心 NMS；M5 决策从 YuNet 换到 SCRFD（检出 23→75/119，M6 修 EXIF 方向后 109/119） |
| 人体姿态 | **YOLOv8n-pose**（[Xenova/yolov8n-pose](https://huggingface.co/Xenova/yolov8n-pose)） | 640×640 输入、/255 归一化、输出 [1, 56, 8400]；SCRFD 漏检时用头部关键点定位主体区域 |
| 并行 | `rayon` | JPG 解码 + 像素指标并行分块；AI 推理经 Mutex 串行（实测池化无收益）；上万张走分块并行 |
| 缓存 | `rusqlite`（bundled） | 按 (path, size, mtime, CACHE_VERSION, 配置指纹) 缓存；命中跳过解码与推理 |
| XMP 写出 | 自写轻量 XML 侧车 | 只写 `xmp:Rating` + `firstcut:` 命名空间存子分；他人侧车（无 `firstcut` 标记）不覆盖 |
| 序列化/日志 | `serde`+`csv` | CSV 报告 + 进度日志到 stderr |

> ARW 解码（rawler/rawloader）**本期不做** —— 已确认用 JPG 评分、分数映射到同名 ARW。后续若想精确分析动态范围再加。

## 3. 评分引擎（5 评分维度 + 连拍去重）【已实现】

1. **清晰度/合焦**：下采样 1024px → Tenengrad（Sobel 梯度方差）÷ 亮度方差归一化（消除场景纹理差异），饱和曲线映射（k=800k，实测范围 12 万~172 万）。
   - **M5 主体感知三层兜底**：SCRFD 人脸命中 → 人脸区域 1.5× 框内 reblur P80（避开皮肤平滑区，捕眼睛/发丝锐边）；SCRFD 漏检 → YOLOv8-pose 头部关键点扩展 1.4×1.6× 区域 reblur P80；都无 → 50 分中性下限（大光圈浅景深照片不误判，真糊由人工 gallery 复核）。
2. **曝光**：直方图过曝（≥250）与欠曝（≤5）像素比例（4× 系数惩罚）+ 判定亮度偏离理想值的 **EV 容差带**。
   - **M6 决策：从"码值高斯"改为"EV 容差带"**。旧做法（平均亮度偏离 `exposure_target=128` 的高斯衰减）会把两类**合法**场景判成曝光失误：舞台黑幕布把全图均值拉到 40 以下、白裙白背景把全图均值推到 200 以上。
   - 容差带按曝光档位定义：±1 EV（码值 92~176）内满分（AE 正常波动范围），超出后线性衰减，暗侧到 -4 EV、亮侧到 +2 EV 降为 0。**两侧独立**（`ev_full_lo/hi`、`ev_lo/hi`）：舞台放宽暗侧但不放宽亮侧，雪景反之。亮侧更陡是因为高光溢出在 JPG 里不可恢复，暗部在 RAW 里通常还能救。
   - **主体感知单向修正**：有主体级人脸（高度 ≥ 4%）时取最亮的一张主体脸区域均值，把判定值往中灰方向拉（夹在"全图 ~ 中灰"之间，不越过中灰）。所以暗色/亮色误检框不会把正常照片打下去，同时暗背景/亮背景场景能被救回。
   - 全图侧仍用**截尾均值**（排除最暗 25% 像素）作为基准。
3. **噪点**：暗部（<40）8×8 块标准差 **P15**（最平滑暗块，避开暗部场景纹理污染）+ ISO 容忍度曲线 `k = K0·(1 + 0.3·log10(iso/100))`，`K0=3.0`。
4. **构图**：SCRFD 人脸框 → 三分法交点（4 点）距离（最大 0.47）+ 人脸高度占比（8%~30% 理想，<8% 线性递减至 0.5，>30% 视为怼脸降至 0.75）+ 多人降权（1→1.0、2-4→0.95、5+→0.85）；无人脸给中性 60 分（不惩罚风景/静物）。**只统计主体级人脸（高度 ≥ 4%）**：贴纸脸/背景路人不参与，避免误判。
5. **美学**：CLIPIQA+ 0-100 分（224×224、CLIP 归一化、sigmoid×100）；M5 从 MUSIQ 换入，分布区分度提升至 26-76 区间。
6. **连拍去重**：按 `DateTimeOriginal` 时间戳聚类（间隔 ≤2s 为一组，`keep_k=2`）→ 组内 dHash 感知哈希（9×8 → 64 bit、汉明距离 ≤10 为同一子簇）→ 子簇内按总分排序保留 top-K 并标记"组内第 N 名 / 是否保留"。

**汇总权重（默认，M5 决策 A+修复）**：清晰 0.30 / 曝光 0.25（M5 从 0.20 提升）/ 噪点 0.15 / 构图 0.15 / 美学 0.15（和 = 1.0）。总分 0-100 + 5 个子分 + 人脸数全部进 CSV。

## 4. 流水线设计（上万张性能）

```
scan（读 EXIF 建索引，SQLite 增量）
  → analyze（解码+像素指标，rayon 并行，box 降采样 ~1024px）
  → detect（CLIPIQA + SCRFD + 姿态，ort 推理，缓存命中跳过）
  → dedup（时间聚类 + dHash，组内排序）
  → score（加权汇总）
  → output（XMP 星级 + CSV）
```

- 缓存命中即跳过（SQLite，键 = path+size+mtime+CACHE_VERSION+**配置指纹**），重跑只处理新照片。
  - 配置指纹（`config_fingerprint`）是 M6 修复的一个静默 bug：此前换 `--config` 后缓存仍然命中，用户改了权重/曲线却看到完全一样的结果。现在配置变化自动重算。
- 实测吞吐：119 张 33MP（含 3 模型）约 18.4s（16 核）；增量重跑秒级。1 万张冷跑约 26 分钟。
- 处理中不移动/删除任何文件，只写 XMP 侧车和 CSV（安全）。

## 5. XMP 输出约定

- 为每张照片写侧车 `<stem>.xmp`（`DSC00001.xmp`），含 `xmp:Rating`（1-5 星）+ `firstcut:` 命名空间（5 维子分/人脸/连拍信息）。同一 stem 的 JPG/ARW 共用一个侧车。
- **命名兼容性（M7 已解决）**：`<stem>.xmp` 是 **Lightroom/ACR** 的约定，**darktable 也读**该格式（它自己的 `<stem>.<ext>.xmp` 也认，见 [darktable 文档](https://docs.darktable.org/usermanual/4.2/en/overview/sidecar-files/sidecar-import/)）→ 一份侧车两边通用。此前写成 `<stem>.<ext>.xmp`，Lightroom 读不到。
- 他人侧车（无 firstcut 命名空间，如 LR 写的调色参数）**不覆盖**，只提示跳过。
- **星级（M7）**：默认 `star_mode = "relative"`，按本次批次内百分位给星（10/30/65/90）。绝对阈值模式保留（`rating_5..2`）。理由见 §10。
- CSV 每行：文件、拍摄时间、相机、ISO/光圈/快门、5 维子分、总分、**星级**、人脸数、连拍组号、组内排名、建议操作。

## 6. 项目结构

```
pic_process/
├── Cargo.toml
├── firstcut.toml            # 通用评分配置模板（config-template 生成）
├── presets/                 # 场景预设（编译进二进制，config-template --preset 输出）
│   ├── portrait.toml  stage.toml  highkey.toml  sports.toml  lowlight.toml
├── models/                  # 首次下载的 ONNX 模型（gitignore）
├── src/
│   ├── main.rs              # clap CLI（scan / score / config-template [--preset]）
│   ├── scan.rs              # 扫描 + EXIF + JPG/ARW 配对
│   ├── decode.rs            # JPEG box 降采样解码 + EXIF 方向 + 灰度/直方图
│   ├── metrics/             # sharpness / exposure / noise / composition
│   ├── ai/                  # iqa(CLIPIQA) / facedetect(SCRFD) / pose(YOLOv8)
│   ├── dedup.rs             # 连拍分组 + dHash 聚类 + 排序
│   ├── score.rs             # 分析调度 + 加权汇总 + 缓存接入
│   ├── output/              # csv.rs / xmp.rs
│   ├── cache.rs             # SQLite 增量缓存（键含配置指纹）
│   ├── config.rs            # 权重/曲线/星级/曝光 EV 容差 + 场景预设（--config 可配）
│   └── lib.rs
├── src/bin/                 # tune / gallery / debug_scrfd / debug_pose / debug_exposure / probe
└── tests/
```

## 7. 里程碑（每步可交付、可验证）

- ✅ **M0 骨架**：CLI + 目录扫描 + EXIF + CSV 输出（268 张真实照片实测通过）
- ✅ **M1 像素指标**：清晰度/曝光/噪点 + 加权总分（纯算法，无 AI 依赖）
- ✅ **M2 连拍去重**：时间聚类 + dHash 子簇聚类 + 组内排序 top-K（6 项单元测试）
- ✅ **M3 AI 接入**：CLIPIQA 美学 + SCRFD 人脸检测 + 构图维度，5 维评分（后期从 MUSIQ/YuNet 升级，见 M5）
- ✅ **M4 输出完善**：XMP 星级侧车（`<名>.<原扩展名>.xmp`，xmp:Rating + firstcut 子分，他人侧车保护）+ SQLite 增量缓存（size+mtime+版本键，二次运行 16/16 命中 0.78s；M6 追加配置指纹）
- ✅ **M5 调参与验证**：性能优化（21.5s→10.6s/119张）、噪点 P15 修复、浅景深清晰度误判修复（主体感知三层链路：SCRFD 人脸区域 reblur → YOLOv8-pose 头部区域 → 50 中性下限）、人脸漏检换 SCRFD（检出 23→75/119）、gallery 联系表；**用户决策落地**：A=多场景配置文件（--config/config-template，权重+曲线+星级+曝光目标可配）、B=星级放宽 75/60/45/30、C=换 CLIPIQA（美学分布 26-76，区分度提升）
- ✅ **M6 缺陷修复**（2026-07，见 §10 决策记录）：AI 预处理通道顺序 bug、EXIF 方向未处理、构图被贴纸脸/背景脸污染、曝光模型改为 EV 容差带 + 主体感知单向修正、缓存键缺配置指纹、场景预设落地
- 🔄 **M7 交付与兼容**（2026-07，见 §10）：XMP 侧车命名改 Lightroom 约定、星级改批次内相对分档、配对键含目录、连拍排序用实际权重；**待办**：LR 真实导入验证、自适应连拍保留、ground truth 校准

> 当前状态（Phase 1 完成 + M6/M7 修复）：`pic_process score <目录> [--xmp] [--config x.toml] [--cache <文件>] [--no-ai] [-k N]`，26 项单元测试 + 6 项集成测试全过；CLIPIQA 冷跑 ~155ms/张（含 SCRFD+姿态）。

## 8. 风险与开放问题（当前状态）

- **美学模型**：~~MUSIQ~~ → **已定案：CLIPIQA+ ONNX**（M5 切换，86Cao/IQA-ONNX-Models，224×224、CLIP 归一化、sigmoid×100，分布区分度 26-76 高于 MUSIQ 的 36-65；已下载至 `models/clipiqa_model.onnx{,.data}`）。
- **人脸检测**：~~YuNet~~ → **已定案：SCRFD 10g**（M5 切换，RuteNL/SCRFD-face-detection-ONNX，640×640、9 输出解码、阈值 0.3 + 贪心 NMS；119 张真实照片检出 23→75，M6 修 EXIF 方向后 109/119）。
- **onnxruntime Windows GPU**：DirectML provider 支持 OK；无 GPU 时自动回落 CPU。AI_POOL_SIZE=1（Mutex 串行）——实测池化无收益（AI 非瓶颈且每 session 线程减半变慢）。
- **权重校准**：默认权重 0.30/0.25/0.15/0.15/0.15（和=1.0，M5 决策 A：曝光 0.25 压欠曝虚高）；多场景用 `--config` 按需调整，内置 `presets/` 5 份（portrait/stage/highkey/sports/lowlight）。
- **Lightroom 读 XMP 星级**：命名已改为 LR 约定（`<stem>.xmp`），但**仍缺一次真实导入验证**（用户在 LR 里导入目录，确认星级/子分显示）。Phase 1 唯一没被实测过的交付物。
- **配对键（M7 已修）**：此前所有索引/配对/侧车去重都只用"文件名主干"，而索尼编号在 DSC09999 后回绕 → 上万张跨目录必然出现同名文件，会让分数/连拍/侧车互相覆盖。现改为「目录 + 主干」配对键。
- **遮挡脸/超大脸漏检**（未解决）：口罩/头盔遮挡时 SCRFD 置信度会掉到 0.2 附近，超大人脸特写也会漏检（PORTRAIT_TEST 0.117）。当前对评分的影响已通过"主体感知单向修正 + 构图只计主体脸"降级处理，未换模型。

## 9. Phase 2（远期）：批量 RAW 开发，替代 Lightroom 手动流程

> 需求：选片后不想进 LR，希望自动完成"自动曝光/色调 + 镜头校正 + AI 降噪"批量出图。
> 结论（已调研）：**可行**，采用"Rust 编排 + darktable-cli 引擎"分工。

### 9.1 开源方案调研结论

| 需求 | 方案 | 状态 |
|---|---|---|
| 批量 RAW 开发引擎 | [darktable-cli](https://darktable-org.github.io/dtdocs/en/special-topics/program-invocation/darktable-cli/)（无头批处理，Windows 有官方构建，索尼 ARW 支持好） | ✅ 成熟 |
| 镜头校正 | [lensfun](https://github.com/lensfun/lensfun)（darktable 内置，开源镜头数据库）；冷门头可自校准（[lens_calibrate](https://gitlab.com/cryptomilk/lens_calibrate)） | ✅ 成熟，覆盖视镜头而定 |
| AI 降噪 | darktable 5.0 [neural restore 模块](https://darktable-org.github.io/dtdocs/en/module-reference/utility-modules/shared/neural-restore/)（ONNX Runtime 后端，含 RAW 降噪方向，[PR #20854](https://github.com/darktable-org/darktable/pull/20854) 在做 Bayer 域 RawNIND） | ✅ 5.0 已内置，演进中 |
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
     - neural restore: 按 ISO 分级降噪强度
     - 白平衡: 按场景（日光/阴天/自动）简单预设
  2. 循环调用 darktable-cli 批量导出：
     - 16bit TIFF（归档，保留后期空间）
     - 高质量 JPG（分享/预览）
  3. 输出到 developed/ 目录，不动原片
```

### 9.3 Phase 2 风险

- **lensfun 对用户镜头的覆盖**：需实测；缺失时用 lens_calibrate 自校准（一次性的活）。
- **neural restore 的 RAW 降噪**（RawNIND）可能尚未合入正式版，需在实施时确认 darktable 版本能力；不可用则退回 darktable 传统 profiled 降噪（效果仍可接受）。
- **darktable 读 XMP 侧车的字段**：需用真实照片验证一遍（M4 阶段已计划验证 XMP 星级，可一并做）。
- **GPU 需求**：neural restore 走 ONNX Runtime，无 GPU 会慢，需确认机器配置。

### 9.4 镜头清单与校正策略（已确认镜头）

用户主力镜头：FE 24-70mm F2.8 GM II（SEL2470GM2）、Sigma 50mm F1.4 DG DN Art（E 口）、Sony 70-350mm F4.5-6.3 G OSS（SEL70350G）、后续添置 Sony 200-600mm F5.6-6.3 G OSS（SEL200600G）。

**关键背景**：
- darktable 的 lens correction 模块只走 **lensfun 数据库**，按 EXIF 自动匹配，匹配不到则"无 profile"（[官方文档](https://darktable-org.github.io/dtdocs/en/module-reference/processing-modules/lens-correction/)、[社区反馈](https://github.com/darktable-org/darktable/issues/11022)）；内嵌校正数据（embedded DNG corrections）仅 DNG 支持（[PR #12880](https://github.com/darktable-org/darktable/pull/12880)），**ARW 不适用**。
- 索尼 ARW 的 maker notes 里**内嵌镜头校正数据**（畸变/暗角/色差），RawTherapee 的 Lens/Geometry 可读取（[参考](https://photo.stackexchange.com/questions/114615/raw-therapee-lens-geometry-correction-sony-a6100/114621)）→ 可作**备用引擎**。
- 24-70 GM II 光学素质极高（畸变极小，Adobe 早期都无官方 profile、依赖内嵌数据）→ 即使无 lensfun profile，跳过校正也可接受。

**lensfun 覆盖实测（2026 直连 lensfun master 分支数据库文件核对）**：

| 镜头 | lensfun 覆盖 | 校正数据类型 | 结论 |
|---|---|---|---|
| FE 24-70 GM II | ✅ 命中 | 畸变(ptlens) ✅ 色差(poly3) ✅ 暗角 ❌ 缺 | 用 lensfun；GM II 暗角极轻，可接受；不满意再用内嵌数据补暗角 |
| Sigma 50/1.4 DG DN | ❌ **未命中**（mil-sigma.xml 无此条目） | — | 兜底：RawTherapee 内嵌数据；仍不行→跳过（Art 系畸变极小） |
| 70-350G | ✅ 命中 | 畸变 ✅ 暗角(pa) ✅ 色差 ✅ 全套 | 直接用 lensfun |
| 200-600G | ✅ 命中（此前 Affinity 论坛帖时代缺失，现已加入） | 畸变 ✅ 暗角(pa) ✅ 色差 ✅ 全套 | 直接用 lensfun |

**兜底链路（按优先级）**：darktable lensfun 命中 → RawTherapee 读索尼内嵌数据（同一张 ARW 换引擎出图）→ lens_calibrate 自校准 → 跳过（仅限畸变可忽略的镜头）。当前实际只需要对 **Sigma 50/1.4 DG DN** 走兜底。

**对 Phase 1 无影响**：索尼 JPG 出厂即烘焙镜头校正，评分用 JPG 天然已校正。

### 9.5 里程碑追加（Phase 2 在 Phase 1 M5 之后）

- **P2-M1**：环境验证 —— 安装 darktable，darktable-cli 手动跑通一张 ARW 全流程（校正+降噪+导出 TIFF/JPG）
- **P2-M2**：Rust 编排器 —— 生成 XMP 侧车 + 调用 darktable-cli + 并发批处理 + 进度/日志
- **P2-M3**：联动 Phase 1 —— 曝光补偿值写入侧车、按评分阈值决定开发名单
- **P2-M4**：实测调优 —— 你的镜头跑一轮，调 lensfun 覆盖、降噪强度分级、输出验证

## 10. 决策记录（M6，2026-07）

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

## 11. 交付方式

Phase 1 已交付（v1.0/v1.1 已发布 Release）；Phase 2 规划经确认后再开工。
每次改动必须同步更新 `README.md` / `DESIGN.md`，不允许文档与实现状态不一致。
