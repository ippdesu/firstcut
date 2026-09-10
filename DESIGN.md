# 照片初筛评分工具（Rust）— 设计与实现文档

> 状态：**Phase 1 已完成并发布 v1.1**（GitHub: ippdesu/firstcut，tag v1.0 / v1.1）；M6/M7/M8 缺陷修复 + M9 自适应连拍保留 + M-UI1 复核界面已实现（分支交付，未另发 Release）
> 日期：2026 规划稿 / 2026 实施完成
> 需求来源：索尼相机 JPG+ARW 连拍/风景/人像选片地狱，需要自动初步评分
> 配套文档：`README.md`（用户手册）/ `release_notes.md`（版本说明）/ `REVIEW.md`（外部评审记录与处理状态）/ `M5_REVIEW.md`（M5 决策记录）/ `firstcut.toml`（配置模板）/ `P2_M0.md`（Phase 2 环境验证清单）

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
| CLI | `clap` | 子命令：`scan`（建索引）/ `score`（分析+评分）/ `config-template`（生成配置模板）/ `review`（本地 Web 复核界面，见 §12）。**`report` / `download-models` 已规划但未实现**——CSV 由 `score` 直接产出；模型缺失时给出下载链接并自动降级。 |
| JPG 解码 | `jpeg-decoder`（快速路径）+ `image` 兜底 | JPEG 全解码后 **box 块平均降采样**到 ~1MP 分析尺寸（~50ms/张 33MP；jpeg-decoder 0.3 无 DCT 缩放，故全解码+块平均） |
| EXIF | `kamadak-exif` | ISO、光圈、快门、拍摄时间（连拍聚类用）；多值 ASCII 字段取首个非空值 |
| 像素处理 | `image` / 自写 | 灰度（BT.601 加权）、直方图、Sobel 梯度、3×3 box blur（未用 `imageproc`）；EXIF Orientation 1-8 在降采样后应用（5/7 为"先旋转后镜像"，见 §10 M8-3） |
| AI 推理 | `ort` 2.0.0-rc.13（onnxruntime-rs，静态链接自包含，无需 DLL） | 跑 CLIPIQA + SCRFD + YOLOv8-pose；实验性 `--gpu`（DirectML，`--features gpu` 构建，实测 119 张 16.7s vs CPU 17.4s，无显著收益，不默认开启）；CPU 池化收益不显著 |
| 美学评分 | **CLIPIQA+ ONNX**（[86Cao/IQA-ONNX-Models](https://huggingface.co/86Cao/IQA-ONNX-Models)） | 224×224、CLIP 归一化、`(x/255 - mean) / std`、sigmoid 输出 ×100 → 0-100 分；M5 决策从 MUSIQ 换到 CLIPIQA（区分度更高） |
| 人脸检测 | **SCRFD 10g**（[RuteNL/SCRFD-face-detection-ONNX](https://huggingface.co/RuteNL/SCRFD-face-detection-ONNX)） | 640×640 输入、(x-127.5)/128 归一化、9 个输出张量（score/bbox/**kps** × stride [8,16,32]，布局 [N,dim] 行主序、score 已 sigmoid）；阈值 0.3 + 贪心 NMS；M5 决策从 YuNet 换到 SCRFD（检出 23→75/119，M6 修 EXIF 方向后 109/119）；M9 起解码 kps（5 关键点）做姿态描述子 |
| 人体姿态 | **YOLOv8n-pose**（[Xenova/yolov8n-pose](https://huggingface.co/Xenova/yolov8n-pose)） | 640×640 输入、/255 归一化、输出 [1, 56, 8400]；SCRFD 漏检时用头部关键点定位主体区域 |
| 并行 | `rayon` | JPG 解码 + 像素指标并行分块；AI 推理经 Mutex 串行（实测池化无收益）；上万张走分块并行 |
| 缓存 | `rusqlite`（bundled） | 按 (path, size, mtime, CACHE_VERSION, 配置指纹) 缓存；命中跳过解码与推理 |
| XMP 写出 | 自写轻量 XML 侧车 | 只写 `xmp:Rating` + `firstcut:` 命名空间存子分；他人侧车（无 `firstcut` 标记）不覆盖 |
| 序列化/日志 | `serde`+`csv` | CSV 报告 + 进度日志到 stderr |

> ARW 解码（rawler/rawloader）**本期不做** —— 已确认用 JPG 评分、分数映射到同名 ARW。后续若想精确分析动态范围再加。

## 3. 评分引擎（5 评分维度 + 连拍去重）【已实现】

1. **清晰度/合焦**：下采样 1024px → Tenengrad（Sobel 梯度方差）÷ 亮度方差归一化（消除场景纹理差异），饱和曲线映射（k=800k，实测范围 12 万~172 万）。
   - **M5 主体感知三层兜底**：SCRFD 人脸命中 → 人脸区域 reblur P80（半宽/半高 = 脸框尺寸 ×1.5，即实际区域约 3× 脸框；避开皮肤平滑区，捕眼睛/发丝锐边）；无主体级人脸（含"检出的脸全部 <4%"）→ YOLOv8-pose 头部关键点包围盒 ×1.4/×1.6 区域 reblur P80；都无 → 50 分中性下限（大光圈浅景深照片不误判，真糊由人工 gallery 复核）。
   - 区域分与全局分**取高者**（不是"命中即用区域分"）：区域估计偶发偏低时不至于把整张拉下去；代价是跑焦但背景纹理繁杂的照片可能被全局分救回。
2. **曝光**：直方图过曝（≥250）与欠曝（≤5）像素比例（4× 系数惩罚）+ 判定亮度偏离理想值的 **EV 容差带**。
   - **M6 决策：从"码值高斯"改为"EV 容差带"**。旧做法（平均亮度偏离 `exposure_target=128` 的高斯衰减）会把两类**合法**场景判成曝光失误：舞台黑幕布把全图均值拉到 40 以下、白裙白背景把全图均值推到 200 以上。
   - 容差带按曝光档位定义：±1 EV（码值 92~176）内满分（AE 正常波动范围），超出后线性衰减，暗侧到 -4 EV、亮侧到 +2 EV 降为 0。**两侧独立**（`ev_full_lo/hi`、`ev_lo/hi`）：舞台放宽暗侧但不放宽亮侧，雪景反之。亮侧更陡是因为高光溢出在 JPG 里不可恢复，暗部在 RAW 里通常还能救。
   - **主体感知单向修正**：有主体级人脸（高度 ≥ 4%）时取最亮的一张主体脸区域均值，把判定值往中灰方向拉（夹在"全图 ~ 中灰"之间，不越过中灰）。所以暗色/亮色误检框不会把正常照片打下去，同时暗背景/亮背景场景能被救回。
   - 全图侧仍用**截尾均值**（排除最暗 25% 像素）作为基准。
3. **噪点**：暗部（<40）8×8 块标准差 **P15**（最平滑暗块，避开暗部场景纹理污染）+ ISO 容忍度曲线 `k = K0·(1 + 0.3·log10(iso/100))`，`K0=3.0`。
4. **构图**：SCRFD 人脸框 → 三分法交点（4 点）距离（最大 0.47）+ 人脸高度占比（8%~30% 理想，<8% 线性递减至 0.5，>30% 视为怼脸降至 0.75）+ 多人降权（1→1.0、2-4→0.95、5+→0.85）；无人脸给中性 60 分（不惩罚风景/静物）。**只统计主体级人脸（高度 ≥ 4%）**：贴纸脸/背景路人不参与，避免误判。多人降权计数用 **5%** 门槛（4%~5% 的脸计构图分但不计合影人数）。
5. **美学**：CLIPIQA+ 0-100 分（224×224、CLIP 归一化、sigmoid×100）；M5 从 MUSIQ 换入，分布区分度提升至 26-76 区间。
6. **连拍去重**（M2 + M9 自适应保留）：按 `DateTimeOriginal` 时间戳聚类（间隔 ≤2s 为一组）→ 组内 dHash 感知哈希（9×8 → 64 bit、汉明距离 ≤10 为同一子簇）→ **保留单元**内按总分排序保留 top-K（默认 3）并标记"组内第 N 名 / 是否保留"。
   - **M9 自适应保留（默认开启，`dedup.adaptive_keep=false` 回退 M2 行为）**：dHash 子簇内再按 SCRFD 关键点姿态描述子聚类——5 个关键点（双眼/鼻/双嘴角）按人脸框归一化成 10 维向量（对位置/尺度不变），欧氏距离 > `pose_cluster_threshold`（默认 0.25，定标：棚拍同姿势 0.02~0.06 / 跨姿势 0.46~0.51 双峰）判为不同姿势，**每个姿势簇各自保留 top-K**。解决"30fps 连拍不同姿势被 dHash 判重只留 2 张"的问题。
   - 无人脸/关键点退化的帧共享一个伪簇（保留行为与 M2 等价）；单组保留总量受 `burst_group_cap`（默认 20）上限，超出按总分截断。
   - **永不删除/移动任何文件**：`burst_keep` 只是建议标记。

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
  - 配置指纹（`config_fingerprint`）是 M6 修复的一个静默 bug：此前换 `--config` 后缓存仍然命中，用户改了权重/曲线却看到完全一样的结果。
  - 指纹**只覆盖影响缓存值的曲线参数**（`sharpness_k`/`noise_k0`/`exposure_*`）；权重与星级阈值在运行期合成总分/星级，改它们不会触发重算（M8 性能修正）。
  - `flush` 只写本次新增/更新的行（dirty set），不再每次重写全部行。
- 实测吞吐：119 张 33MP（含 3 模型）约 17.4s（16 核，~146ms/张）；**M9 实测 1447 张真实图库（含 1272 张连拍）冷跑 2m55s（~121ms/张）**；增量重跑秒级（1447 张全命中 2.06s）。1 万张冷跑约 20 分钟。
- 缓存行含姿态描述子（M9，`pose_desc BLOB`，10×f32）；`CACHE_VERSION` 13。
- 处理中不移动/删除任何文件，只写 XMP 侧车和 CSV（安全）。
- CSV 含 `analysis_ok` 列（M8 遗留项，M9 落地）：解码失败/无配对 ARW 可过滤，运行结束 stderr 给失败清单。

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
│   ├── dedup.rs             # 连拍分组 + dHash 聚类 + M9 姿态聚类 + 排序
│   ├── score.rs             # 分析调度 + 加权汇总 + 缓存接入 + 姿态描述子
│   ├── output/              # csv.rs / xmp.rs
│   ├── cache.rs             # SQLite 增量缓存（键含配置指纹，含 pose_desc）
│   ├── config.rs            # 权重/曲线/星级/曝光 EV 容差 + [dedup] + 场景预设（--config 可配）
│   ├── review/              # M-UI1：本地 Web 复核服务（axum + 内嵌前端）
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
- ✅ **M8 评审修复**（2026-07，见 §10）：外部评审（GLM）13 项问题逐条核实并修复——配对回归（P0）、配置 fail-fast、EXIF 5/7 互换、清晰度兜底缝隙、指纹收窄、缓存 flush 收窄、文档批次修正、pose sigmoid 定案
- ✅ **M9 自适应连拍保留**（2026-09，见 §7.1 / §10）：SCRFD kps 解码 + 姿态描述子缓存化 + dHash 子簇内姿态聚类（每姿势簇保留 3 张、单组上限 20、默认启用可关闭）；阈值 0.25 经棚拍双峰定标；archive 119 张回归星级逐档一致（12/24/41/31/11）；搭车：`analysis_ok` 列、实验性 `--gpu`（DirectML，无显著收益）
- ✅ **M-UI1 复核界面**（2026-09，见 §12）：`pic_process review <目录>` 本地 Web 界面——缩略图墙 + 筛选排序 + 1:1 原图灯箱 + 连拍组并排同步缩放对比；只读
- 🔄 **M7 遗留待办**：LR 真实导入验证、ground truth 校准

### 7.1 M9 自适应连拍保留（已实现）：按姿态自适应保留连拍

**问题**：`keep_k` 固定、按 dHash 子簇保留。30fps 连拍时同一个人的**不同姿势/表情**会被 dHash 判为近似重复，只留 2 张 → 丢掉不同瞬间。

**实现**（利用已有算力，零额外推理成本）：
1. SCRFD `scrfd_10g_bnkps` 的 **5 个关键点（双眼/鼻/双嘴角）**在 M9 起参与解码（kps 张量 [N,10]，格中心 + offset×stride，与 bbox 同一距离约定；debug_scrfd 实测 40 点 100% 落框内自检）。按人脸框归一化成 10 维**姿态描述子**（`score::pose_descriptor`），对位置/尺度不变。
2. 描述子进缓存（`pose_desc BLOB`，`CACHE_VERSION` 13）。
3. dHash 子簇内**贪心种子姿态聚类**（与 dHash 聚类同风格、扫描顺序确定）：距离 > 0.25 → 新姿势簇；无描述子帧共享伪簇。
4. 每个**姿势簇**各自保留 top-K（默认 3）；单组保留总量上限 20（超出按总分截断）；`adaptive_keep=false` 一键回退 M2 行为（逐字节一致）。

**定标**：棚拍 18 帧同主体连拍——同姿势两两距离 0.02~0.06、跨姿势 0.46~0.51，双峰清晰，0.25 居中（两侧余量 4×+）；舞台连拍（萤火虫）姿态连续变化时无真空带，0.25 给出合理"瞬间"粒度。

**规模实测**：真实图库 1447 张（88% 在连拍组）——姿态簇规模分布 1~9 张、组保留数 ≤ 20，行为符合预期；M9 只改 burst 列，五维分与星级与 M8 完全一致（回归断言）。

> 当前状态（Phase 1 + M6/M7/M8/M9 + M-UI1）：`pic_process score <目录> [--xmp] [--config x.toml] [--cache <文件>] [--no-ai] [-k N] [--gpu]`，42 项单元测试 + 6 项集成测试全过；冷跑 ~121-146ms/张（含 SCRFD+姿态，16 核）。

## 8. 风险与开放问题（当前状态）

- **美学模型**：~~MUSIQ~~ → **已定案：CLIPIQA+ ONNX**（M5 切换，86Cao/IQA-ONNX-Models，224×224、CLIP 归一化、sigmoid×100，分布区分度 26-76 高于 MUSIQ 的 36-65；已下载至 `models/clipiqa_model.onnx{,.data}`）。
- **人脸检测**：~~YuNet~~ → **已定案：SCRFD 10g**（M5 切换，RuteNL/SCRFD-face-detection-ONNX，640×640、9 输出解码、阈值 0.3 + 贪心 NMS；119 张真实照片检出 23→75，M6 修 EXIF 方向后 109/119）。
- **onnxruntime Windows GPU**：DirectML provider 支持 OK；无 GPU 时自动回落 CPU。AI_POOL_SIZE=1（Mutex 串行）——实测池化无收益（AI 非瓶颈且每 session 线程减半变慢）。**M9 实验性 `--gpu`（`--features gpu` 构建）实测：119 张 16.7s vs CPU 17.4s（~4%）——单张小批量 DML 吃不满 GPU，无显著收益，保持实验性、默认关闭**。
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

**两个易踩的坑（已排除/需注意）**：
- `18-135mm` 与 `23mm f/1.4` 在 `mil-fujifilm.xml` 里也有同名条目（`XF18-135mmF3.5-5.6R LM OIS WR`、`XF23mmF1.4 R`），**那是富士 X 口，不是命中**；两支镜头各自另有真正的 Sony E 口条目。
- **不要假定"命中 Sony E 口 = 三项校正齐全"**：`FE 24-70 GM II` 缺暗角；`FE 70-200mm f/2.8 GM OSS` 只有畸变。通用判定逻辑必须**逐条目解析 `<calibration>`**，不能只看是否命中。

**兜底链路（按优先级）**：darktable lensfun 命中 → RawTherapee 读索尼内嵌数据（同一张 ARW 换引擎出图）→ lens_calibrate 自校准 → 跳过（仅限畸变可忽略的镜头）。

> ⚠️ **`lensfun-update-data` 类的升级路径救不了**：已抓上游 master 验证，
> **两支适马（50/1.4 DG DN、70-200 DG DN OS）上游同样没有条目**，只能自建 profile 或跳过。
> 真正"完全无 lensfun"的合计 **1522 张 / 8603 = 17.7%**。
> 建议把"两支适马是否进入上游"做成**周期性检查项**。

### 9.5 里程碑追加（Phase 2 在 Phase 1 M5 之后）

- **P2-M1**：环境验证 —— 安装 darktable，darktable-cli 手动跑通一张 ARW 全流程（校正+降噪+导出 TIFF/JPG）
- **P2-M2**：Rust 编排器 —— 生成 XMP 侧车 + 调用 darktable-cli + 并发批处理 + 进度/日志
- **P2-M3**：联动 Phase 1 —— 曝光补偿值写入侧车、按评分阈值决定开发名单
- **P2-M4**：实测调优 —— 你的镜头跑一轮，调 lensfun 覆盖、降噪强度分级、输出验证

### 9.6 实施清单（待你补充/确认）

> 勾选项 = 还没做。请直接在上面增删，或告诉我哪几条要拆细。

**P2-M0 环境与可行性（先做，决定后面能不能走）**
- [ ] 装 darktable 5.x（Windows 官方构建），确认版本 ≥ 5.0（neural restore 需要）
- [ ] `darktable-cli` 命令行跑通一张 ARW → TIFF/JPG
- [ ] `darktable-cltest` 确认 OpenCL/GPU 状态；neural restore 的 ONNX 后端是否可用
- [ ] 确认 neural restore 是否有 RAW 域降噪（Bayer 域 RawNIND 是否已合入正式版）
- [ ] lensfun 对 4 支镜头（24-70 GM II / Sigma 50 DG DN / 70-350G / 200-600G）的覆盖实测

**P2-M1 侧车模板与字段映射**
- [ ] 确定写哪些模块：lens correction(auto) / exposure(补偿) / neural restore(强度) / 白平衡
- [ ] 手工造一份侧车，验证 darktable 确实读取并生效（**关键：确认字段名和写法**）
- [ ] Phase 1 的 EV 偏移 → darktable exposure 模块参数换算（注意 darktable 用 EV，含黑电平补偿）
- [ ] 降噪强度分级策略（按 ISO？按 Phase 1 噪点分？）

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
- [ ] 输出只要 JPG，还是 TIFF 归档 + JPG 都要？
- [ ] 要不要保留 darktable 的 `.xmp` 调色参数（方便以后手改）？注意这会让目录多一批文件
- [ ] 白平衡：完全自动 / 按场景预设 / 保留机内？
- [ ] 降噪强度按 ISO 分级，还是按 Phase 1 的噪点分分级？
- [ ] 是否额外导出"高分片名单"（CSV/文本）给 LR 用？

## 10. 决策记录（M6 / M7 / M8 / M9，2026-07 ~ 2026-09）

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

## 11. 交付方式

Phase 1 已交付（v1.0/v1.1 已发布 Release）；Phase 2 规划经确认后再开工。
每次改动必须同步更新 `README.md` / `DESIGN.md`，不允许文档与实现状态不一致。

## 12. UI 规划（M-UI1 复核 + M-UI2 操作台，已实现）

> 需求来源：gallery 是按 119 张验证规模做的（base64 内嵌、单文件 HTML、每次全量重新解码），
> 上万张时 HTML 达数百 MB 且无法交互对比；选片复核需要"缩略图看不清是否合焦时
> 能随时开 100% 原图、连拍组内并排对比"。

### 12.1 选型结论（2026-09 定案）

**axum 本地服务 + 内嵌 vanilla JS 前端（浏览器即客户端）**。

| 候选 | 结论 |
|---|---|
| **axum + 浏览器**（选定） | 纯 cargo、零 npm 工具链、单 exe 交付不变；图片密集体验是浏览器主场；HTML/JS 对 AI 协作开发最友好；HTTP API 将来可平移进 Tauri |
| Tauri 2（Rust+WebView2） | 若将来要"双击即开的桌面壳"再包一层（前端资产可复用）；本轮为它引入 Node 工具链不值得 |
| egui / Slint | 立即模式/声明式 GUI 做大图缩略图墙+自由缩放对比的开发成本高，弃 |
| Dioxus | Rust 写 UI心智成本高，生态不如直接写 JS，弃 |

### 12.2 M-UI1 已实现范围（复核视图）

- **形态**：`pic_process review <目录> [--config x.toml] [--cache x.sqlite] [--port 8787]` → 本地服务（绑 127.0.0.1）+ 自动开浏览器。
- **数据流**：不依赖 CSV——`scan_directory` + 按当前配置指纹过滤缓存行 → 内存快照；**星级/连拍信息在启动时用当前配置现算**（与 `score` 同一函数，保证逐张一致）；缓存未命中的照片显示"未评分"。
- **API**：`GET /`（内嵌前端）、`GET /api/photos`（JSON 快照）、`GET /thumb?p=`（320px 缩略图，按需生成落盘 `<照片根>/.firstcut/thumbs/`，已存在按 size+mtime 跳过）、`GET /image?p=`（原图直读，100% 预览零预生成成本）。
- **安全**：仅 127.0.0.1；所有 `p` 参数 canonicalize 后强制在扫描根目录内（越界 403）；扩展名白名单。
- **前端**（无构建步骤）：缩略图墙（分页 500/页 + lazy）；筛选（星级/保留/五维下限/有人脸/文件名搜索）+ 排序，条件持久化 localStorage；**单图 1:1 灯箱**（滚轮缩放 + 拖拽平移 + 双击复位，`←/→` 同连拍组切换）；**连拍组并排对比**（2~4 窗格共享同一 transform，滚轮/拖拽同步缩放，检查合焦的眼睛）。

### 12.3 M-UI2 已实现范围（操作台）

- **score 流程抽库**：`score::run_score_job(dir, cfg, opts, on_event)`——`score` 子命令
  与 UI 跑批共用同一实现（重构等价性验收：CSV 逐字节一致）；进度经回调上报。
- **UI 内跑批**：侧栏触发，全局单任务互斥，`/api/job` 轮询进度与日志；
  完成后快照热替换（无需重启服务）。
- **UI 内改星**：`/api/rate` 写 XMP 侧车（只改 Rating、firstcut 子分保留、
  他人侧车 409 保护）；跑批进行中禁改。
- **配置编辑**：`/api/config` GET/POST，`toml_edit` 保留注释与字段顺序；
  写盘前经 `load_config_text` 全量校验，非法值拒绝。
- **扫描器防污染**：`scan_directory` 跳过 `.firstcut/` 缓存子树
  （缩略图不会被当成照片重新评分——M-UI2 实测发现的 bug）。

### 12.4 后续里程碑

- **M-UI3 Phase 2 集成**：develop 编排（P2-M2）的可视化——开发名单、进度、结果对比。
