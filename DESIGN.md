# 照片初筛评分工具（Rust）— 设计与实现文档

> 状态：**Phase 1 已完成并发布 v1.0**（GitHub: ippdesu/firstcut，tag v1.0）
> 日期：2026 规划稿 / 2026 实施完成
> 需求来源：索尼相机 JPG+ARW 连拍/风景/人像选片地狱，需要自动初步评分
> 配套文档：`README.md`（用户手册）/ `release_notes.md`（版本说明）/ `M5_REVIEW.md`（M5 决策记录）

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
| CLI | `clap` | 子命令：scan（建索引）/ score（分析+评分）/ config-template（生成配置模板）；CSV 由 score 直接产出 |
| JPG 解码 | `jpeg-decoder`（快速路径）+ `image` 兜底 | JPEG 全解码后 **box 块平均降采样**到 ~1024px（~50ms/张 33MP；无 DCT 缩放，0.3 版库已移除该特性） |
| EXIF | `kamadak-exif` | ISO、光圈、快门、拍摄时间（连拍聚类用）；多值 ASCII 字段取首个非空值 |
| 像素处理 | `image` / 自写 | 灰度（BT.601）、直方图、Sobel 梯度、3×3 box blur、8×8 块统计 |
| AI 推理 | `ort` 2.0.0-rc.13（onnxruntime-rs，静态链接自包含，免 DLL） | CLIPIQA + SCRFD + YOLOv8-pose；GPU（DirectML）未启用 |
| 美学评分 | **CLIPIQA+ ONNX**（[86Cao/IQA-ONNX-Models](https://huggingface.co/86Cao/IQA-ONNX-Models)） | 224×224、CLIP 归一化 (x/255 - mean)/std、输出 0-100；M5 决策从 MUSIQ 换成（区分度 36-65 → 26-76） |
| 人脸检测 | **SCRFD 10g**（[RuteNL/SCRFD-face-detection-ONNX](https://huggingface.co/RuteNL/SCRFD-face-detection-ONNX)） | 640×640、(x-127.5)/128、distance(l,t,r,b)×stride 格中心解码、score 已 sigmoid、阈值 0.3 + 贪心 NMS；M5 决策从 YuNet 换成（检出 23→75/119） |
| 人体姿态 | **YOLOv8n-pose**（[Xenova/yolov8n-pose](https://huggingface.co/Xenova/yolov8n-pose)） | 640×640、/255 归一化、[1,56,8400] 通道优先布局（score 已 sigmoid、坐标已像素空间）；SCRFD 漏检时头部关键点定位主体区域 |
| 并行 | `rayon` | JPG 解码 + 像素指标并行；AI 推理经 Mutex 串行（实测池化无收益） |
| 缓存 | `rusqlite`（bundled） | 按 (path, size, mtime, CACHE_VERSION) 缓存；命中跳过解码与推理；并行段用预加载快照 |
| XMP 写出 | 自写轻量 XML 侧车 | `<名>.<原扩展名>.xmp`，`xmp:Rating` + `firstcut:` 命名空间；他人侧车（无 firstcut 标记）不覆盖 |
| 序列化/日志 | `serde`+`csv` | CSV 报告；进度日志到 stderr（未用 tracing） |

> ARW 解码（rawler/rawloader）**本期不做** —— 已确认用 JPG 评分、分数映射到同名 ARW。后续若想精确分析动态范围再加。

## 3. 评分引擎（5 评分维度 + 连拍去重）【已实现】

1. **清晰度/合焦**（主体感知三层链路）：
   - SCRFD 检出人脸 → 人脸框 1.5× 区域 reblur 差分 **P80**（合焦边缘证据；皮肤平滑不稀释）
   - 人脸漏检 → YOLOv8-pose 头部关键点包围盒（鼻/眼/耳 1.4× 扩展）区域 reblur P80
   - 都无主体 → 全局 Tenengrad÷亮度方差，并给 **50 分中性下限**（大光圈浅景深照片不被误判；用户确认其场景均为浅景深人像/动物）
2. **曝光**：直方图过曝（≥250）与欠曝（≤5）像素比例 + 平均亮度偏离**目标亮度**（默认 128，exposure_target 可配）惩罚。
3. **噪点**：暗部（<40）8×8 块标准差 **P15 低百分位**（最平滑暗块≈传感器噪声，避免场景纹理污染）+ ISO 容忍度曲线 k=3.0·(1+0.3·log10(iso/100))。
4. **构图**：SCRFD 人脸框（无人脸时 pose 人体框）→ 三分法交点距离 + 主体大小占比（2%~30% 理想区间）+ 多人降权；无主体给中性 60 分。
5. **美学**：CLIPIQA 0-100 分（224×224、CLIP 归一化，直接映射）。
6. **连拍去重**：按 `DateTimeOriginal` 时间戳聚类（间隔 ≤2s 为一组）→ 组内 dHash 感知哈希（汉明距离 ≤10 为同一子簇）→ 子簇内按总分排序保留 top-K（默认 2）并标记"组内第 N 名"。

**汇总权重（默认，M5 校准）**：清晰 0.35 / 曝光 0.20 / 噪点 0.15 / 构图 0.15 / 美学 0.15。总分 0-100 + 5 个子分 + 人脸数全部进 CSV。

## 4. 流水线设计（上万张性能）

```
scan（读 EXIF 建索引，SQLite 增量）
  → analyze（解码+像素指标，rayon 并行，box 降采样 ~1024px）
  → detect（CLIPIQA + SCRFD + 姿态，ort 推理，缓存命中跳过）
  → dedup（时间聚类 + dHash，组内排序）
  → score（加权汇总）
  → output（XMP 星级 + CSV）
```

- 缓存命中即跳过（SQLite，键 = path+size+mtime+版本号），重跑只处理新照片。
- 实测吞吐：119 张 33MP（含 3 模型）约 18.4s（16 核）；增量重跑秒级。1 万张冷跑约 26 分钟。
- 处理中不移动/删除任何文件，只写 XMP 侧车和 CSV（安全）。

## 5. XMP 输出约定

- 为每张照片写同名侧车（`DSC00001.ARW.xmp`、`DSC00001.JPG.xmp`，按 Lightroom 命名约定），含 `xmp:Rating`（1-5 星，默认分档 75/60/45/30，可配）+ `firstcut:` 命名空间（5 维子分/人脸/连拍信息）。
- 他人侧车（无 firstcut 命名空间，如 LR 写的调色参数）**不覆盖**，只提示跳过。
- CSV 每行：文件、拍摄时间、相机、ISO/光圈/快门、5 维子分、总分、人脸数、连拍组号、组内排名、建议操作。

## 6. 项目结构

```
pic_process/
├── Cargo.toml
├── firstcut.toml            # 评分配置模板（config-template 生成）
├── models/                  # 首次下载的 ONNX 模型（gitignore）
├── src/
│   ├── main.rs              # clap CLI（scan / score / config-template）
│   ├── scan.rs              # 扫描 + EXIF + JPG/ARW 配对
│   ├── decode.rs            # JPEG box 降采样解码 + 灰度/直方图
│   ├── metrics/             # sharpness / exposure / noise / composition
│   ├── ai/                  # iqa(CLIPIQA) / facedetect(SCRFD) / pose(YOLOv8)
│   ├── dedup.rs             # 连拍分组 + dHash 聚类 + 排序
│   ├── score.rs             # 分析调度 + 加权汇总 + 缓存接入
│   ├── output/              # csv.rs / xmp.rs
│   ├── cache.rs             # SQLite 增量缓存
│   ├── config.rs            # 权重/曲线/星级/曝光目标（--config 可配）
│   └── lib.rs
├── src/bin/                 # tune（调参）/ gallery（联系表）/ debug_scrfd / debug_pose / probe
└── tests/
```

## 7. 里程碑（每步可交付、可验证）

- ✅ **M0 骨架**：CLI + 目录扫描 + EXIF + CSV 输出（268 张真实照片实测通过）
- ✅ **M1 像素指标**：清晰度/曝光/噪点 + 加权总分（纯算法，无 AI 依赖）
- ✅ **M2 连拍去重**：时间聚类 + dHash 子簇聚类 + 组内排序 top-K（6 项单元测试）
- ✅ **M3 AI 接入**：CLIPIQA 美学 + SCRFD 人脸检测 + 构图维度，5 维评分（后期从 MUSIQ/YuNet 升级，见 M5）
- ✅ **M4 输出完善**：XMP 星级侧车（`<名>.<原扩展名>.xmp`，xmp:Rating + firstcut 子分，他人侧车保护）+ SQLite 增量缓存（size+mtime+版本键，二次运行 16/16 命中 0.78s）
- ✅ **M5 调参与验证**：性能优化（21.5s→10.6s/119张）、噪点 P15 修复、浅景深清晰度误判修复（主体感知三层链路：SCRFD 人脸区域 reblur → YOLOv8-pose 头部区域 → 50 中性下限）、人脸漏检换 SCRFD（检出 23→75/119）、gallery 联系表；**用户决策落地**：A=多场景配置文件（--config/config-template，权重+曲线+星级+曝光目标可配）、B=星级放宽 75/60/45/30、C=换 CLIPIQA（美学分布 26-76，区分度提升）

> 当前状态（Phase 1 完成）：`pic_process score <目录> [--xmp] [--config x.toml] [--cache <文件>] [--no-ai] [-k N]`，12 项单元测试全过；5 星 24/119、4 星 74、3 星 21（新分档）；CLIPIQA 冷跑 ~155ms/张（含 SCRFD+姿态）。

## 8. 风险与开放问题（当前状态）

- **美学模型**：MUSIQ → **已换 CLIPIQA+**（类别度 36-65 → 26-76，详见 M5_REVIEW.md 决策 C）。
- **onnxruntime GPU**：DirectML 启用属于后续优化（当前 CPU ~155ms/张，1 万张冷跑 ~26 分钟；批量场景建议）。
- **权重校准**：默认权重经真实照片验证（清晰 0.35/曝光 0.20/噪点 0.15/构图 0.15/美学 0.15，和=1.0）；多场景用 `--config` 按需调整。
- **Lightroom 读 XMP 星级**：侧车命名与 `xmp:Rating` 均按 LR 约定；建议实机验证一次（可在 Phase 2 环境验证时一并做）。

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

## 10. 交付方式

本规划经确认后生效；**确认前不写任何代码**。确认"开始做"后进入 M0。
