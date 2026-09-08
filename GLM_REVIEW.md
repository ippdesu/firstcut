# 项目评审报告（glm-review 分支）

> 评审日期：2026-09-08　|　评审基准：main @ ab768d8　|　评审人：GLM
> 方式：通读 DESIGN.md / README.md / 全部 src 源码与测试，对照文档逐项核实；
> 关键结论均以实机运行验证（`cargo test` + 对 testpic 实跑 `scan` / `score`），
> 并核对了 image crate 0.25.10 与 Cargo.lock 中依赖的真实源码语义。
> 本分支只含本报告，未改动任何代码。

---

## 总评

项目文档质量高，DESIGN/README 与实现的一致性总体良好（26 项单元测试实测全过，
M6 的 EV 容差带数学、M7 的侧车命名/相对星级实现均与文档吻合）。
但存在 **1 个未发现的 P0 级功能回归**（M7 配对键改动静默破坏了分目录工作流，
且集成测试在 HEAD 上是挂的）、**3 个 P1 正确性问题**（含一个与 M6-4 教训同类的
静默回退），以及若干文档/精度/性能问题。

---

## 🔴 P0 — 功能回归（实测复现）

### 1. M7「配对键含目录」破坏了 JPG/RAW 分目录工作流：ARW 拿不到分数

**位置**：`src/scan.rs:93`（`pair_key`）、`src/main.rs:177-186`（分数映射）

**证据（实机复现，testpic 采用 `JPG/`、`RAW/` 两个子目录分放——M0 时代起的标准布局，
集成测试 fixture 也按此写）**：

- `pic_process scan testpic` → **全部 31 个文件 `has_pair=false`**（JPG 与 ARW
  不在同一目录，永不配对）。
- `pic_process score testpic` → CSV 中 **15 个 ARW 行的分数/星级/连拍字段全部为空**，
  `DSC00886.ARW` 等没有任何来自同名 JPG 的映射分数。
- 集成测试 `test_scan_directory_finds_photos` 在 HEAD 上 **失败**（5 过 1 挂）：
  断言 `paired_count > 0`（tests/integration_test.rs:34）。M7 提交（db68d2b）
  改了 scan.rs 但没有更新该测试；README/DESIGN 仍声称"6 项集成测试全过"。

**影响**：索尼双卡分工（槽 1 存 JPG / 槽 2 存 RAW）或手动把 JPG、ARW 分开存放
的用户——这是本仓库自己的测试 fixture 所建模的工作流——核心承诺
「对 JPG 评分、把分数映射到同名 ARW」（DESIGN §0）**静默失效**：不报错、
CSV 里 ARW 行空白、`--xmp` 模式下一个侧车都不会写出。

**分析**：M7-3 决策要解决的问题是"编号回绕导致不同目录出现**不同**照片同名"
（-roll1/roll2 场景），方案"目录+主干"正确；但把**同一张照片的 JPG/ARW 分放
两个子目录**的情形一并排除了。两种场景需要区分。

**修复方向**：同目录配对优先；无同目录配对时，跨目录同名、且全树内该主干
无歧义（各自只有一个 JPG 和一个 ARW 候选）时兜底配对（可再加拍摄时间校验）。
同步更新 M7-3 决策记录、测试 fixture 与 README。

---

## 🔴 P1 — 正确性问题

### 2. `--config` 加载失败时静默回退默认配置，继续跑完整流水线

**位置**：`src/main.rs:101-105`

```rust
Err(err) => {
    eprintln!("[score] 警告: 配置加载失败（使用默认）: {err:#}");
    ScoreConfig::default()
}
```

用户显式指定了 `--config`（意味着"我要用这份参数"），TOML 拼错/路径错时只在
stderr 提一句，然后用**默认配置**完成评分并**写出 XMP 星级侧车**。
这是 M6-4 决策记录里刚修过的"静默错误"的同类坑，且后果更重（错误星级落盘）。
应当 `bail` 直接退出。

### 3. EXIF Orientation 5/7 的变换互换（镜像翻转）

**位置**：`src/decode.rs:107-109`

```rust
5 => rotate90(&flip_horizontal(&img)),
7 => rotate270(&flip_horizontal(&img)),
```

已对照 image crate 0.25.10 `imageops/affine.rs` 源码确认 rotate90/270 的精确
语义，并对 2×2 矩阵逐步推演：

- Orientation 5 要求 **transpose**（沿左上-右下主对角线翻转）= `flip_horizontal(rotate90(img))`；
- 代码给出的 `rotate90(flip_horizontal(img))` 是 **anti-transpose**（沿反对角线翻转），
  恰好是 orientation **7** 的正确变换；
- Orientation 7 反之，两个 case 内外顺序写反、互换了。

影响：orientation 5/7 的照片呈镜像+倒置，人脸检测/构图/曝光全部系统性偏差。
索尼机身常写 1/3/6/8，5/7 少见，故为低频但真实的 bug。
（6/8/2/3/4 已逐一验证正确；另已验证 `image::open` 兜底路径不会自动应用方向，
无双重旋转问题。）

### 4. 配置的另外两个静默陷阱：未知字段被忽略 + star_mode 无校验

**位置**：`src/config.rs`（`#[serde(default)]`，无 `deny_unknown_fields`）

- TOML 里 `exposure_ev_Io = 3`（l 手滑成 I）会被 **静默忽略**，暗侧容差保持
  默认 4.0——用户以为改了，结果不变。这正是 M6-4 修过的"改了配置看不到变化"
  的变体，只是这次出在解析层。
- `star_mode = "absolue"`（拼错）静默按 relative 处理（`assign_ratings` 只
  区分 absolute/其他）。建议 `#[serde(deny_unknown_fields)]` + star_mode
  白名单校验（与权重和、EV 嵌套校验放一起）。

---

## 🟡 P2 — 语义/精度问题

### 5. 清晰度三层链路与文档语义不一致 + 链路有缝隙

**位置**：`src/score.rs:225-241, 252, 277-283`

- **(a) max 语义未入文档**：有主体级人脸时代码取 `max(region, global)`，
  DESIGN §3.1 写的是"人脸命中 → 人脸区域 reblur P80"。当主体真糊而背景纹理
  清晰（跑焦+繁杂背景）时 global 盖过 region——这恰是清晰度维度（尤其
  sports 预设、权重 0.45）想抓的废片。是决策还是笔误需要定案并写进文档。
- **(b) 三层链路缝隙**：pose 兜底条件是 `faces == 0`；SCRFD 只检出
  高度 4%~100% 以下的小脸（如 3%）时 `faces > 0`，**跳过 pose 层**直接落
  `max(global, 50)` 中性下限。与 M5"SCRFD 漏检 → pose"的意图不完全一致。
- **(c) 命名误导**：`reblur_mean_region`（sharpness.rs:171）实际返回 **P80**
  （函数内 doc 也写 P80），建议改名 `reblur_p80_region`。
- 另注：无人脸时 `sharpness.max(50.0)` 地板会把真糊的无主体照片也抬到 50+
  ——这是 M5 已明文记录的取舍（宁可漏判、人工 gallery 复核），不算 bug，
  但与 sports 预设"清晰度决定成败"的定位存在张力，可在预设注释里提一句。

### 6. pose.rs 两处注释自相矛盾（输出是否已 sigmoid）

**位置**：`src/ai/pose.rs:9`（"cls 与 kps conf 为 logits 需 sigmoid"）
vs `src/ai/pose.rs:88`（"Xenova 转换：score 已 sigmoid"）。代码两处都没做 sigmoid。

若该模型输出实为 raw logits，则 `CONF_THRESHOLD=0.25` 实际对应 sigmoid≈0.56、
`head_region` 的 `c > 0.3` 对应≈0.57，均比设计意图严格约一倍（方向是漏检而非
错杀）。需用 `pic_process-probe` / `debug_pose` 实测输出值域定案，并统一注释。

### 7. AI 预处理 `resize_exact` 直接压扁长宽比（无 letterbox）

**位置**：`src/ai/facedetect.rs:57`、`src/ai/pose.rs:59`（640×640）、
`src/ai/iqa.rs:36`（224×224）

3:2 照片被压扁 1.5:1 后送模型。归一化坐标映射**没有**因此出错（水平/垂直各自
归一化，已验证），但模型看到的是变形的脸——SCRFD 的选型理由恰是小脸检出，
变形会折损这部分精度；CLIPIQA 美学分同理。标准做法是 letterbox 补边。

---

## 🟡 P2 — 性能

### 8. 配置指纹过宽：只调权重也会全量重算

**位置**：`src/config.rs:149-176`（`config_fingerprint`）

指纹把 `weights` 和 `rating_5..2` 也编了进去，但缓存行存的是五维子分+dHash+
faces——权重与星级阈值都只在运行期合成总分/星级时才用到，**不影响缓存值**。
后果：用户最高频的调参动作（改权重）会让全部照片重新解码+AI 推理
（155ms/张，万张约 26 分钟）。指纹应收窄为 `sharpness_k / noise_k0 /
exposure_target / exposure_ev_* / exposure_subject_blend`。

顺带两个缓存小项：

- `ScoreCache::flush()`（cache.rs:135）每次把内存里**全部**行（含未变化的旧行）
  INSERT OR REPLACE 一遍，且永不清理已删除文件的行；
- 缓存命中判定逻辑在 `score.rs:122-135` 内联重复了一份，`cache.rs get()`
  已无人调用（死代码）。两处条件将来改一处漏一处，建议收敛为一份。

---

## 🔵 P3 — 文档与实现不一致（DESIGN §11 明文要求不允许）

| # | 位置 | 问题 |
|---|---|---|
| 9 | `release_notes.md` | 停在 v1.1，缺 M6/M7 全部内容；"12 单元测试"现为 26 项；"Lightroom 直接可读"在 v1.1 时点并不成立（当时仍是 darktable 命名，M7 才改）。M6/M7 也未按惯例发 Release notes 更新 |
| 10 | `tests/integration_test.rs:43-49` | 注释仍是 M5 中间方案"sharpness 0.35 → 和 1.05"，现行 0.30/1.0；断言区间 0.95~1.10 与 `load_config` 的 ±0.05 口径不一致 |
| 11 | `README.md` CSV 列说明 | `burst_size` 写"组内张数"，实际是 **dHash 子簇内**张数（30 帧连拍可能 burst_size=2，用户会误解） |
| 12 | `DESIGN.md` §3.4 vs `composition.rs:58` | 多人降权计数实际用硬编码 **5%** 门槛（主体级人脸定义是 4%），4%~5% 之间的脸计构图分但不计合影人数。行为合理但文档没写 |
| 13 | 实测 | 集成测试当前 1 项失败（见 P0-1），README/DESIGN 均声称"6 项全过" |

---

## 🔵 P4 — 小项（不阻塞，备忘）

- 集成测试依赖本地 testpic 内容，fixture 漂移无防护（本次就漂了）；`#[ignore]`
  改造已在 Phase 2 TODO 里，建议提前。
- `decode.rs:41` JPEG 魔数判断第一个分支 `head == [FF D8 FF DB]` 被第二个分支
  包含，冗余。
- `models/` 目录与默认缓存文件均相对 CWD：换目录运行 score 找不到模型/缓存。
- `pair_key` 对目录做小写化：Windows 无碍，大小写敏感 FS 上仅大小写不同的
  两个目录会同键冲突（极端边缘）。
- 相对星级在极小批次下分布畸形（2 张 → 5★+3★，永不出现 1★；1 张 → 5★）。
  行为可接受，README 可提一句"建议整场一次跑完"已有，可再加"批次过小时星级失真"。
- `SessionPool` 盲轮询：池 >1 时不 try_lock 空闲 session（现状 AI_POOL_SIZE=1
  无影响；将来若扩池需注意）。
- 噪点 ISO 解析失败静默按 100（更严格方向）；EXIF 展示值对非数字挡位的机型
  会走这条路。
- XMP `firstcut:` 未写 `burstSize`、无元数据时间戳；他人侧车保护只识别
  "从未被 firstcut 写过"的侧车——用户在 LR 里改过星级的 firstcut 侧车再跑
  `--xmp` 会被覆盖（符合"自己的侧车可覆盖"的设计，但值得在 README 提醒）。

---

## ✅ 评审中验证为正确/一致的项（记录在案）

- 26 项单元测试实测全过，与文档数量一致。
- M6 EV 容差带数学（sRGB↔EV 换算、容差带嵌套、两侧独立、主体单向修正）实现
  与测试、文档三方吻合；`exposure_score` 夹取逻辑正确。
- M7 XMP 命名（`<stem>.xmp`、保留大小写）、相对星级（并列平均位次）、
  侧车保护、`-k` 连拍排序用实际权重——实现与决策记录一致。
- 缓存键（path+size+mtime+CACHE_VERSION+配置指纹）与文档一致；
  `image::open` 兜底路径无 EXIF 双重旋转问题（已读 crate 源码确认）。
- 构图三分法距离上限 0.47、噪点 P15、dHash 严格性测试均正确。

## 建议的处理顺序

1. **P0-1 配对回归**：决定分目录工作流是否为支持目标（建议是：fixture 本来
   就是这么建的），改配对策略 + 修测试 + 补 M7-3 决策记录。
2. **P1-2 / P1-4**：`--config` 失败改为硬错误；serde `deny_unknown_fields`
   + star_mode 校验。两处都是小改动、高收益。
3. **P1-3 orientation 5/7**：对调内外顺序，补 2×2 矩阵单元测试。
4. **P2-8 指纹收窄** + 清理 `cache.rs get()` 死代码。
5. P2-5/6/7 与 P3 文档项按 M8 一并处理。
