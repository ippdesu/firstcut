# 项目评审记录（外部评审逐条工单 + 处理状态）

> **本文件是长期评审记录**：每轮外部评审（GLM / 其他）的工单与处理状态都追加在这里，
> 不再单开文件。原始工单保持原样，处理结论统一记在下面的「处理状态」表。
>
> **当前状态**：第 1 轮（GLM，2026-09-08）**13 项全部核实为真、全部已修复**，
> 合入 main @ `b477c96`。测试 26 → **37 单元 + 6 集成**全过。

## 追加新一轮评审的格式

1. 在「处理状态」区新增一张表：`工单 | 问题 | 核实结论 | 状态`；
   核实结论要写**实测依据**（命令输出、矩阵推演、值域采样），不采信"看起来对"。
2. 在本文件末尾新增一节 `## 第 N 轮原始工单（<来源>，<日期>）`，原样保留对方工单。
3. 暂不实施/待拍板的项写进该轮的「附」小节，不要静默丢弃。
4. 同步更新 `README.md` / `DESIGN.md`（见 `DESIGN.md` §11 的交付纪律）。

## 处理状态（第 1 轮 · GLM · 2026-09-08）

| 工单 | 问题 | 核实 | 状态 |
|---|---|---|---|
| PR-1 | 配对回归：`JPG/` + `RAW/` 分目录时 ARW 拿不到分数 | ✅ 成立（P0，M7-3 引入） | ✅ 已修：两步配对（同目录优先 + 无歧义跨目录兜底） |
| PR-2 | `--config` 失败只警告；未知字段静默忽略；`star_mode` 拼错静默回退 | ✅ 全部成立 | ✅ 已修：硬报错 + `deny_unknown_fields` + 取值/百分位校验 |
| PR-3 | EXIF Orientation 5/7 变换互换 | ✅ 成立（3×2 矩阵推演确认） | ✅ 已修 + 8 方向像素测试；`CACHE_VERSION` 10→12 |
| PR-4a | pose 兜底在"只有小脸"时被跳过 | ✅ 成立 | ✅ 已修（`sharpness_region.is_none()`） |
| PR-4b | 区域分/全局分取 max 未入档 | ✅ 成立（文档缺失） | ✅ 已补文档，行为不变 |
| PR-4c | `reblur_mean_region` 实际返回 P80 | ✅ 成立 | ✅ 改名 `reblur_p80_region` |
| PR-5a | 配置指纹含权重/星级阈值 → 改权重触发全量重算 | ✅ 成立（实测已改善） | ✅ 已修：指纹只覆盖曲线参数 |
| PR-5b | 缓存命中判定重复、`ScoreCache::get` 死代码 | ✅ 成立 | ✅ 已收敛为 `CacheRow::matches` |
| PR-5c | `flush` 每次重写全部行 | ✅ 成立 | ✅ 已改为只写 dirty 行 |
| PR-6 | 文档批次修正 | ✅ 全部成立 | ✅ 已修 |
| PR-7 | pose 输出是否需 sigmoid 注释矛盾 | ✅ 矛盾成立；**实测 248 个置信度全在 (0,1) → 已 sigmoid** | ✅ 只统一注释，代码不变 |

**附A（暂不实施，等拍板）**：letterbox 预处理（需配合人脸检出率回归，基线 109/119）、
清晰度改"命中即用区域分"、相对星级小批次畸形、他人侧车覆盖边界、低危备忘项。

**附B（评审判定为正确的项）**：与实现核实一致，未发现误判。

**流程教训**：PR-1 能进 main，是因为 M7 提交时跑了 `cargo test` 但用
`Select-Object -First 3` 截断了输出，只看到 lib 结果就误判"全过"。
此后测试结果必须看完整尾部。

---

## 第 1 轮原始工单（GLM，2026-09-08）

> **本文件用途**：交给实现方（DSH）逐条落地为 PR 的工单。每条问题包含：
> 精确位置（file:line）→ 现象与证据 → 根因 → 修复方案（含代码草图）→
> 验收标准 → 文档同步项。实现时若行号偏移，请按符号名定位。
>
> **基准**：main @ `ab768d8`（本分支与其同源，未改任何代码）
> **评审日期**：2026-09-08　**评审方式**：通读 DESIGN/README/全部源码与测试；
> 所有 🔴/🟡 级结论均经实机验证（cargo test + 对 testpic 实跑 scan/score +
> 核对 image crate 0.25.10 依赖源码）。
>
> **实施红线**：
> 1. 遵守 DESIGN.md §11——**每个 PR 必须同步更新 `README.md` / `DESIGN.md`**，不允许文档与实现脱节。
> 2. 不修改任何评分默认值（权重/曲线/星级阈值），除非工单明确要求。
> 3. 涉及缓存值语义变化的 PR **必须递增 `src/cache.rs` 的 `CACHE_VERSION`**（见各条标注）。
> 4. 一个 PR 一个主题；commit message 沿用仓库现有中文 conventional 风格。
> 5. `CACHE_VERSION` 递增冲突的协调：若多个 PR 都需 bump，按合入顺序依次 +1，后合入者 rebase 时调整。

## 验证命令（每个 PR 完成后都要跑）

```bash
cargo test --lib                    # 单元测试（当前 37 项全过，改动后只能增不能减）
cargo test --test integration_test  # 集成测试（当前 6/6）
cargo build --release

# PR-1 手工验收（分目录布局映射）：
./target/release/pic_process scan testpic -o /tmp/scan.csv
#   预期：除无对端者外所有 JPG/ARW 行 has_pair=true
./target/release/pic_process score testpic -o /tmp/score.csv --no-ai --no-cache --cache /tmp/t.sqlite
#   预期：ARW 行的 total_score / stars / faces 与同名 JPG 一致
#   检查列（1-based）：$4=is_raw, $5=has_pair, $19=total_score, $20=stars
```

---

## PR-1【P0】修复配对回归：JPG/RAW 分目录存放时 ARW 拿不到分数

### 问题

- **位置**：`src/scan.rs:93`（`pair_key`）、`src/scan.rs:122-141`（配对构建）、`src/main.rs:167-199`（分数/星级按 pair_key 回填）。
- **现象**（实测）：testpic 布局为 `JPG/*.JPG` + `RAW/*.ARW` 两个子目录（M0 起
  的 fixture 约定，集成测试也按此写）。当前：
  - `scan testpic` → 全部 31 个文件 `has_pair=false`；
  - `score testpic` → 15 个 ARW 行的分数/星级/连拍字段**全部为空**；
  - 集成测试 `test_scan_directory_finds_photos` **失败**（断言 `paired_count > 0`，
    tests/integration_test.rs:34）。M7 提交 db68d2b 改了 scan.rs 但没更新该测试。
- **根因**：M7-3 为解决"编号回绕→不同目录出现**不同**照片同名"，把配对键改成
  「目录+主干」，同时把**同一张照片的 JPG/ARW 分放两个子目录**（索尼双卡分工、
  手动归档的常见形态）的情形一并排除了。两种场景被混淆。
- **影响**：分目录用户的 CSV 里 ARW 全空白、`--xmp` 一个侧车都不写、全程无报错。

### 修复方案（两步配对，保守消歧）

配对解析从"隐式键相等"改为显式的 `pair_id` 分配，在 `scan_directory` 内完成：

1. **同目录配对（主规则，行为不变）**：同一目录内 `stem` 相同的 JPG 与 ARW 配对。
2. **跨目录兜底（新增）**：仅当某个 stem 在**整个扫描树内恰好只有 1 个 JPG 和
   1 个 ARW**、且两者不在同一目录时，跨目录配对。
   歧义（≥2 个同名 JPG 或 ≥2 个同名 ARW 跨目录，即回绕场景）时**保持不配对**，
   宁可不配也不错误合并——回绕场景下各目录内本就有自己的同目录对，不受影响。

实现要点：

```rust
// PhotoEntry 增加字段（注意 serde 跳过，避免污染 CSV 列）：
#[serde(skip_serializing)]
pub pair_id: String,   // 同目录对: 复用 "dir|stem"；跨目录对: "cross:<stem>"

// scan_directory 流程调整为：
//   1) 现有 stem_exts 统计（键=dir|stem）——同目录配对
//   2) 对「无同目录对」的条目，按 stem 聚合全树候选：
//      jpg_count == 1 && arw_count == 1 → 跨目录配对，赋同一 pair_id
//   3) 配对完成后统一回填 has_pair（有对端才为 true）
```

`src/main.rs` 中所有用 `e.pair_key()` 做回填/去重的地方改用 `e.pair_id()`：
- L167 `by_key`（分析结果键）→ 以 JPG 的 `pair_id` 为键；
- L178-186 ARW 回填 → 按 `pair_id` 查；
- L190-199 星级 `rated`/`ratings` → 按 `pair_id`；
- L202-222 XMP 的 `seen` 去重 → **改为按侧车最终路径去重**（而不是 pair_id），
  因为跨目录对的 JPG 与 ARW 各需一份侧车（Lightroom 按图片文件所在目录找
  `<stem>.xmp`）；同目录对路径相同自然只写一次，行为不变。

### 验收标准

- [ ] `cargo test --test integration_test` **6/6 通过**（现有失败用例转绿）。
- [ ] 上面"验证命令"两条手工验收的预期输出成立。
- [ ] 新增单元测试（scan.rs，合成条目即可，不依赖真实照片）：
  - 分目录（JPG/ + RAW/）→ 配对成功；
  - 回绕（roll1、roll2 各含同名 JPG+ARW）→ 各自按同目录配对，**不**跨目录合并；
  - 歧义（1 个 JPG + 2 个跨目录同名 ARW）→ 不配对；
  - 同目录配对行为与现状一致（既有测试 `pair_key_separates_directories` 改造后仍须通过）。
- [ ] 回归确认：同目录混合布局的照片行为与 main 完全一致（分数、星级、侧车数）。

### 文档同步

- `DESIGN.md` §10 M7-3 决策记录**补记**：原方案的副作用与本修复的两步策略
  （补一条 M7-3 修订，说明为何跨目录兜底要加无歧义约束）。
- `README.md`：输出说明处补一句"JPG 与 ARW 分放不同子目录也可配对（同名且
  无歧义时）"。

### 注意事项

- 缓存**不需要** bump `CACHE_VERSION`（单文件分析值与配对无关）。
- 不要用拍摄时间做消歧（留作后续增强），保持本 PR 最小化。

---

## PR-2【P1】`--config` 失败改为硬错误 + 配置解析防静默陷阱

### 问题

- **2a** `src/main.rs:101-105`：`--config` 加载失败只 `eprintln!` 警告，随后用
  **默认配置**跑完整个流水线并写出 XMP 星级侧车。用户显式传了配置却拿到默认
  结果——与 M6-4 决策（"改了配置看不到变化属静默 bug"）同类且后果更重。
- **2b** `src/config.rs`：`ScoreConfig` 系列只有 `#[serde(default)]`，没有
  `deny_unknown_fields`。TOML 字段名手滑（如 `exposure_ev_Io`）被**静默忽略**，
  用户以为改了参数、结果不变——M6-4 的教训重演在解析层。
- **2c** `src/output/xmp.rs:89`（`assign_ratings`）：`star_mode` 只要不是
  `"absolute"`（忽略大小写）就走 relative，拼写错误（`"absolue"`）静默回退。

### 修复方案

```rust
// main.rs（2a）：
Err(err) => anyhow::bail!("配置加载失败: {path:?}\n{err:#}"),
// 注意：在 bail 前不得已默认配置写出任何 CSV/XMP（现状是先加载再扫描，顺序无需调整）。

// config.rs（2b）：三个结构体都加
#[serde(deny_unknown_fields, default)]
// 两个既有测试（all_presets_are_loadable / unknown_preset_is_none）必须仍通过，
// presets/*.toml 与 config_template() 生成的模板只含已知字段，已核实兼容。

// config.rs load_config()（2c）追加校验：
if !(m.star_mode.eq_ignore_ascii_case("relative")
    || m.star_mode.eq_ignore_ascii_case("absolute")) {
    anyhow::bail!("star_mode 只能是 \"relative\" 或 \"absolute\"，当前为 {m:?}（{path:?}）");
}
```

- 建议顺带（同一 PR，小改动）：校验 relative 百分位单调递增
  `star_five_pct <= star_four_pct <= star_three_pct <= star_two_pct`（当前无校验，
  写反会给全批统一星）。

### 验收标准

- [ ] 单元测试：未知字段 → `load_config` 报错（错误信息含字段名）；`star_mode`
  拼错 → 报错；百分位倒序 → 报错；合法配置 + 5 份预设 → 全部照常加载。
- [ ] 手工：`score testpic --config 不存在.toml` 以非零码退出、stderr 有明确
  原因、**不产出** CSV/XMP。
- [ ] `config-template` / `config-template --preset <5 个>` 生成的文件都能被
  `load_config` 接受。

### 文档同步

- `README.md` "多场景配置"一节：补"配置加载失败会直接报错退出（不再回退默认）；
  未知的配置字段会被拒绝（防止拼写错误静默失效）"。
- `DESIGN.md` §10 决策表追加一行（M8 前）：配置解析 fail-fast 决策及依据
  （援引 M6-4）。

### 注意事项

- 对已持有含多余字段配置文件的用户是破坏性变更（以前静默忽略、现在报错）——
  这正是目的，release notes 里要写明。
- 不涉及缓存值，**不需要** bump `CACHE_VERSION`。

---

## PR-3【P1】修复 EXIF Orientation 5/7 变换互换

### 问题

- **位置**：`src/decode.rs:101-113`（`apply_orientation`，问题在 L107/L109 两行）。

```rust
// 现状（错误）：
5 => rotate90(&flip_horizontal(&img)),
7 => rotate270(&flip_horizontal(&img)),
```

- **证据**：已对照 image crate 0.25.10 `imageops/affine.rs` 源码确认旋转语义
  （rotate90 顺时针：old(x,y)→new(h-1-y,x)；rotate270：old(x,y)→new(y,w-1-x)），
  并以 2×2 矩阵逐步推演：
  - Orientation 5 的正确显示变换是 **transpose**（沿主对角线翻转）=
    `flip_horizontal(rotate90(img))`——先转后翻；
  - 代码现状给出的是 **anti-transpose**（反对角线翻转），恰为 orientation **7**
    的正确变换；两者互换。镜像+倒置的照片会让人脸检测/构图/曝光全部系统性偏差。
- 其余值已逐一验证正确：2=flipH、3=rot180、4=flipV、6=rot90CW、8=rot270CW。
- 另已验证 `image::open` 兜底路径（decode.rs:56）不会自动应用方向
  （`ImageReader::decode` → `DynamicImage::from_decoder`，无隐式旋转），
  修复 `apply_orientation` 即可，两条解码路径共用此函数。

### 修复方案

```rust
5 => flip_horizontal(&rotate90(&img)),
7 => flip_horizontal(&rotate270(&img)),
```

### 验收标准

- [ ] 新增单元测试（decode.rs）：构造 2×2 `RgbImage`（四角像素值 1/2/3/4），
  对 orientation 1~8 全部断言输出像素位置（5 → 期望 `[1 3; 2 4]` 转置；
  7 → 期望 `[4 2; 3 1]` 反对角；其余值锁定现状）。
- [ ] `cargo test --lib` 全过。

### 文档同步

- `DESIGN.md` §2"像素处理"行或 §6 decode.rs 说明：补一句"orientation 5/7 为
  先旋转后镜像（v1.2 修复过一次内外顺序写反）"。release notes 记入修复列表。

### 注意事项

- **必须 bump `CACHE_VERSION`（10 → 11）**：5/7 照片的缓存行是按错误方向算的，
  缓存不含 orientation 字段，只能整批失效重建。
- 实际影响频率低（索尼机身常写 1/3/6/8），release notes 中如实注明。

---

## PR-4【P2】清晰度三层链路：补 pose 兜底缝隙 + max 语义入档

### 问题

- **位置**：`src/score.rs:252`（pose 兜底条件）、`src/score.rs:277-283`（取分）、
  `src/metrics/sharpness.rs:171`（命名）。
- **4a 链路缝隙**：pose 兜底条件是 `faces == 0 && sharpness_region.is_none()`。
  SCRFD 检出的人脸**全部**低于主体级门槛（高度 <4%）时 `faces > 0`，pose 层被
  跳过，直接落到中性下限——与 M5"SCRFD 漏检 → pose"的设计意图不符。
- **4b max 语义未入档**：有主体级人脸时代码取 `max(region, global)`
  （L277-279），DESIGN §3.1 与 README 写的是"人脸命中 → 人脸区域 reblur"。
  主体真糊+背景纹理清晰（跑焦）时全局分会盖过区域分——这恰是清晰度维度
  （sports 预设权重 0.45）想抓的废片。**这是行为决策点，见下。**
- **4c 命名误导**：`reblur_mean_region` 实际返回 **P80**（函数内 doc 自己写着
  P80），名字里的 mean 是历史遗留。

### 修复方案

- **4a（本 PR 实施，低风险）**：条件改为

  ```rust
  if sharpness_region.is_none() {
  ```

  （在既有 `if let Some(pp) = &ai.pose` 块内）。效果：SCRFD 只有小脸/SCRFD 出错
  时都跑 pose 定位头部。无人脸行为不变。
- **4b（默认只改文档；行为变更需用户拍板，不在本 PR 做）**：DESIGN §3.1 与
  README 清晰度行补一句："人脸区域分与全局分**取高者**（防止区域估计偶发偏低
  拉低整张）；代价是跑焦+繁杂背景的照片可能被背景纹理救回。"若用户后续决定
  改为"命中即用区域分"，需单独 PR + 全量回归 + `CACHE_VERSION` bump。
- **4c（本 PR 实施）**：函数改名 `reblur_mean_region` → `reblur_p80_region`
  （调用点仅 score.rs:237 与 260 两处 + doc），行为零变化。

### 验收标准

- [ ] 单元测试：合成 `PersonBox`（关键点 conf 混合有 >0.3 与 <0.3）验证
  `head_region` 返回值；4a 的条件变更以集成方式在 testpic 上冒烟
  （faces 数与 sharpness 分布无异常跳变）。
- [ ] `cargo test --lib` 全过；`cargo test --test integration_test` 不劣于修复前。

### 文档同步

- `DESIGN.md` §3.1 / README 清晰度行（4b 的一句话）。
- release notes 记入 4a/4c。

### 注意事项

- 4a 会改变部分照片（仅小脸被检出者）的 sharpness 输入 → **bump
  `CACHE_VERSION`**（合入顺序在 PR-3 之后则 11 → 12）。
- 无人脸时 `sharpness.max(50.0)` 中性地板是 M5 已明文记录的取舍（宁可漏判真糊、
  人工 gallery 复核），**不改**；但可在 README sports 预设注释里提一句该地板
  对无主体题材的影响。

---

## PR-5【P2】配置指纹收窄 + 缓存层清理

### 问题

- **5a** `src/config.rs:149-176`（`config_fingerprint`）：指纹把 `weights` 与
  `rating_5..2` 也编了进去，但缓存行存的是**五维子分+dHash+faces**——权重与
  星级阈值只在运行期合成总分/星级时使用，不影响缓存值。后果：用户最高频的
  调参动作（改权重）触发全量解码+AI 重算（155ms/张）。
- **5b** 缓存命中判定在 `src/score.rs:122-135` 内联重复一份，
  `src/cache.rs:106-117` 的 `ScoreCache::get` 已无人调用（死代码）；两处条件
  （size/mtime/version/cfg_hash）将来改一处漏一处。
- **5c** `src/cache.rs:135-161`（`flush`）：每次把内存中**全部**行（含未变化的
  旧行）INSERT OR REPLACE 一遍；已删除文件的行永不清理，缓存只增不减。

### 修复方案

- **5a**：指纹只保留影响缓存值的参数——`sharpness_k`、`noise_k0`、
  `exposure_target`、`exposure_ev_full_lo/hi`、`exposure_ev_lo/hi`、
  `exposure_subject_blend`。删除 weights 循环与 `rating_5..2`。
  函数 doc 注明"指纹只覆盖参与缓存值的曲线参数；权重/星级阈值每次运行期生效"。
- **5b**：删除 `ScoreCache::get` 与 `ScoreCache` 结构体上的 `cfg_hash` 字段
  （注意：`CacheRow.cfg_hash` 保留，score.rs 内联判定在用）。把内联判定收敛为
  `cache.rs` 提供的单一函数（如 `CacheRow::matches(size, mtime, version, cfg_hash)`），
  score.rs 调用它，消除重复。
- **5c**：`ScoreCache` 增加 `dirty: std::collections::HashSet<String>`，
  `put()` 记录脏键，`flush()` 只写脏行。旧行清理（删除照片后回收）标记为
  **可选项**：实现的话在 main.rs flush 前传入本次扫描的 path 集做差集删除，
  不实现则在 cache.rs 顶部注释说明缓存只增不减的现状。

### 验收标准

- [ ] 单元测试：仅改 weights 的两个配置 → 指纹相同；改 `sharpness_k`/任意
  `exposure_*` → 指纹不同；`rating_5` 改动 → 指纹相同。
- [ ] 手工：同一目录跑两遍 `score`，第二遍全部缓存命中（日志 `新分析 0`）；
  改一个权重字段再跑 → **仍全部命中**（修复前会全量重算）；
  改 `exposure_target` → 全量重算（预期行为）。
- [ ] `cargo test --lib` 全过。

### 文档同步

- `DESIGN.md` §4 缓存小节与 §10 M6-4 行：补注"配置指纹覆盖曲线参数；权重与
  星级阈值不参与缓存键（它们在运行期合成总分/星级）"。
- release notes 记入性能改进（"只调权重不再触发全量重算"）。

### 注意事项

- 5a 会改变指纹值 → 首跑全量 miss 一次，属预期，**不需要**额外 bump
  `CACHE_VERSION`（cfg_hash 本身就是缓存键的一部分）。
- DefaultHasher 的跨版本稳定性不作保证——缓存只用于本地加速，哈希变化最多
  导致一次全量重算，可接受，无需引入稳定哈希。

---

## PR-6【P3】文档批次修正

### 清单（逐项独立小改，可合并为一个 docs PR）

| # | 位置 | 修改 |
|---|---|---|
| 6a | `release_notes.md` | 新增"未发布（M6/M7 合入后）"小节：M6 全部缺陷修复（AI 通道布局、EXIF 方向、构图主体脸门槛、EV 容差带、缓存指纹、场景预设）、M7 全部（侧车命名 `<stem>.xmp`、相对星级、配对键含目录+PR-1 修订、连拍排序用实际权重）、以及 PR-2~PR-5 的用户可见变更（含"配置未知字段现在报错"的破坏性提示）。修正"12 单元测试"→实际数量；删除/修正"Lightroom 直接可读"在 v1.1 时点不成立的表述（当时是 darktable 命名） |
| 6b | `tests/integration_test.rs:41-49` | 注释仍写"sharpness 0.35 → 和 1.05"（M5 中间方案），改为现行 0.30/1.0；断言区间 0.95~1.10 收紧为与 `load_config` 一致的 0.95~1.05 |
| 6c | `README.md` CSV 列说明 | `burst_size` 语义由"组内张数"改为"**dHash 子簇内**张数"（30 帧连拍可能显示 2），并在连拍去重说明段落对齐同一用词 |
| 6d | `DESIGN.md` §3.4 + `src/metrics/composition.rs:56-58` | 写明多人降权计数用 **5%** 门槛（硬编码 0.05，主体级定义是 4%；4%~5% 的脸计构图分但不计合影人数）——代码注释顺手补齐 |
| 6e | `DESIGN.md` §3.1 / README | 补"人脸区域 1.5× 框"的精确几何：实现为半宽/半高 = 脸框尺寸 ×1.5（即实际区域约 3× 脸框）；pose 头部区域为关键点包围盒 ×1.4/×1.6 |

### 验收标准

- [ ] 文档与代码逐条对得上；`cargo test --lib` 全过（6b 改了断言范围）。
- [ ] 抽查：文档中不再出现与实现冲突的数字（测试数、权重、阈值、几何）。

---

## PR-7【P3·需先验证】pose 输出 sigmoid 疑点定案

### 问题

- **位置**：`src/ai/pose.rs:9`（模块头："cls 与 kps conf 为 logits **需 sigmoid**"）
  vs `src/ai/pose.rs:88`（detect() 内："Xenova 转换：score **已 sigmoid**"）。
  两处注释互相矛盾，代码两者都没做 sigmoid。
- **风险**：若该 ONNX 实际输出 raw logits，则 `CONF_THRESHOLD=0.25` 实际对应
  sigmoid≈0.56、`head_region` 的 `c > 0.3` 对应≈0.57——阈值比设计意图严格约
  一倍，方向是漏检（姿态兜底更少触发）而非错杀。

### 实施步骤（先验证后改，禁止跳步）

1. 用 `pic_process-probe` / `pic_process-debug-pose` 对 testpic 若干张打印
   `cls` 与 kps conf 的实际值域：
   - 全部落在 (0,1) → 已 sigmoid，**只统一注释**（删掉"需 sigmoid"的说法）；
   - 出现 >1 或 <0 → raw logits，加 sigmoid（`1/(1+exp(-x))`）后再比较阈值，
     **并 bump `CACHE_VERSION`**（会影响 pose 触发面）。
2. 无论哪种结果，在 `pose.rs` 输出解码处用一句注释锁定事实（值域 + 依据），
   防止再次漂移。

### 验收标准

- [ ] 验证结论（值域截图/日志数字）写进 PR 描述；两处注释矛盾消除。
- [ ] `cargo test --lib` 全过；集成测试不劣化。

---

## 附A：暂不实施（记录在案的开放问题，等用户拍板/后续版本）

| 项 | 说明 |
|---|---|
| AI 预处理 letterbox | SCRFD/pose 640×640、CLIPIQA 224×224 目前 `resize_exact` 压扁长宽比（facedetect.rs:57 / pose.rs:59 / iqa.rs:36）。归一化坐标**不会**因此出错（已验证），但模型看到变形的脸，小脸检出率受损。改进需 letterbox + 坐标反算（x = (x_pad − pad_x)/scale），影响面大，建议独立 PR 并配合检出率回归（当前基线 109/119） |
| 清晰度 max 语义改为"命中即用区域分" | 见 PR-4 4b，行为变更需用户拍板 + 全量回归 |
| 相对星级小批次畸形 | 2 张 → 5★+3★（永不出现 1★）、1 张 → 5★。行为可接受；若要改，在 README"批次"提示处补一句即可 |
| 他人侧车保护的边界 | 用户在 LR 里改过星级的 firstcut 侧车再跑 `--xmp` 会被覆盖（符合"自己的侧车可覆盖"设计）。建议仅在 README 提醒，不改行为 |
| `models/`、默认缓存路径相对 CWD；`pair_key` 目录小写化在大小写敏感 FS 的撞键；`SessionPool` 盲轮询；JPEG 魔数判断冗余分支（decode.rs:41 第一分支被第二分支包含） | 均为低危备忘，可搭任意 PR 顺手处理或不动 |

## 附B：评审中验证为正确/一致的项（无需改动）

- 26 项单元测试实测全过（与文档数量一致）。
- M6 EV 容差带数学（sRGB↔EV 换算、容差带嵌套校验、两侧独立、主体单向修正）实现/测试/文档三方吻合；`exposure_score` 夹取逻辑正确。
- M7 XMP 命名（`<stem>.xmp`、保留大小写）、相对星级（并列平均位次）、侧车保护、连拍排序用实际权重——实现与决策记录一致。
- 缓存键（path+size+mtime+CACHE_VERSION+配置指纹）结构与文档一致；`image::open` 兜底路径无 EXIF 双重旋转问题。
- 构图三分法距离上限 0.47、噪点 P15 选取、dHash 及其严格性测试均正确。
- XMP 渲染无 XML 注入面（写入内容不含用户可控字符串）。
