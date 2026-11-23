# 已停止的 Phase 2 研究档案：darktable 批量 RAW 开发

> **⚠️ 状态：已停止（2026-09-18）。**
> 方向由用户拍板：firstcut 回归**筛选与复核工具本位**——仅输出“建议曝光 EV → XMP”（`firstcut:suggestedEV` 信息字段 + `crs:Exposure2` Lightroom 开发字段，带外才给建议、±2 EV 钳位、1/3 档取整，配置项 `exposure_suggest_cap`），实际 RAW 后期开发交由 Lightroom 完成。
>
> 本档案归档自原 `P2_M0.md`、旧版 `DESIGN.md` §9 及 `docs/HISTORY.md` §5，保留全部技术实测、命令陷阱、镜头普查和实验数据供追溯。文中的“已确认”“待实施”“后续”均为停止前措辞，**所有任务均不在当前计划**；`p2_notes/` 等本机研究目录为本地未入库产物，非发行依赖。现行设计见 [`docs/DESIGN.md`](../DESIGN.md)。

> 原始需求：选片后不想进 LR，希望自动完成“自动曝光/色调 + 镜头校正 + AI 降噪”批量出图。
> 当时调研结论：**可行**，采用“Rust 编排 + darktable-cli 引擎”分工。

## 1. 开源方案调研结论

| 需求 | 方案 | 当时状态 |
|---|---|---|
| 批量 RAW 开发引擎 | [darktable-cli](https://darktable-org.github.io/dtdocs/en/special-topics/program-invocation/darktable-cli/)（无头批处理，Windows 有官方构建，索尼 ARW 支持好） | ✅ 成熟 |
| 镜头校正 | [lensfun](https://github.com/lensfun/lensfun)（darktable 内置，开源镜头数据库）；冷门头可自校准（[lens_calibrate](https://gitlab.com/cryptomilk/lens_calibrate)） | ✅ 成熟，覆盖视镜头而定 |
| AI 降噪 | ~~darktable neural restore~~ **实测不可用于 CLI**（见 §3）→ 改用 `rawdenoise`（RAW 域小波）+ `denoise (profiled)`（主） | ❌ neural restore 仅 GUI；替代方案 ✅ 已实测 |
| 自动曝光/色调 | darktable exposure 模块 auto-exposure + Lua（[autostyle](https://darktable-org.github.io/luadocs/lua.scripts.manual/scripts/contrib/autostyle/)）；Phase 1 的曝光分析直接产出每张补偿值写入 XMP | 🟡 可达成，8 成效果 |
| 备选（不推荐） | 纯 Rust 自研：rawler 解码 + 自写色调映射 + `ort` 跑 NAFNet/SCUNet ONNX（[NAFNet](https://github.com/megvii-research/NAFNet)、[ONNX 权重](https://huggingface.co/qualcomm/NAFNet-DeNoise)） | 色彩科学差距大，工作量巨大 |

## 2. Phase 2 历史架构设想

```text
[Phase 1 评分工具] → 保留照片清单 + 每张曝光补偿建议
        ↓
[Phase 2 Rust 编排器]
  1. 为每张 ARW 生成 XMP 侧车（darktable 可读）：
     - lens correction: auto（lensfun）
     - exposure: 补偿值（来自 Phase 1 分析）
     - denoise: `rawdenoise` + `denoise (profiled)`（按 ISO 调 `strength`；
       **不是** neural restore——它没有 CLI 入口，见 §3）
     - 白平衡: 按场景（日光/阴天/自动）简单预设
  2. 循环调用 darktable-cli 批量导出：
     - 16bit TIFF（归档，保留后期空间）
     - 高质量 JPG（分享/预览）
  3. 输出到 developed/ 目录，不动原片
```

## 3. 风险与 neural restore 实测定案

- **lensfun 对用户镜头的覆盖**：需实测；缺失时用 `lens_calibrate` 自校准（一次性的活）。
- **neural restore 无法用于 CLI 批量（已实测定案，2026-09-09）**：
  模块**存在**且 **RAW 域降噪（RawNIND / PR #20854）确已合入 5.6**，但它是 `dt_lib_module_t`（GUI 工具面板）**而非 iop**：不接收 XMP 参数、不在 pixelpipe 里、靠人点 Process 触发，自己跑后台任务写新 DNG/TIFF 再重新导入，源码里没有 `dt_dev_add_history_item`。
  `darktable-cli` 侧证据：① `--help` 无任何 AI/模型选项；② `--luacmd` 脚本**从不执行**（Lua 走 `dt_lua_async_call()` 异步投递，而 CLI 是同步直线流程从不进主循环，`init_gui=FALSE` 还导致 `dt_lib_init()` 不跑、模块连注册都没发生）；③ 二进制里确有 `darktable.ai` Lua API，但只有 GUI 版接受 `--luacmd`。**点一次 GUI 也救不回来**（不是一次性下载问题）。
  → **替代方案（已实测）**：`rawdenoise`（RAW 域小波，轻）+ `denoise (profiled)`（主）。两者都是标准 iop、可走 XMP。**`denoise (profiled)` 内置相机噪声 profile 并按 ISO 自动插值**，“按 ISO 分级降噪”是它的原生行为，`strength` 即分级主旋钮。modversion / `op_params` 二进制布局见本地 `p2_notes/neural_restore.md`；验证方式：同源同参数只改 `<darktable:enabled>` 1→0，两个 16bit TIFF 的 md5 不同。保留未来路径：若上游给 neural restore 加 CLI 入口再重评。
- **darktable 读 XMP 侧车的字段**：需用真实照片验证一遍。
- **GPU 需求**：neural restore 走 ONNX Runtime，无 GPU 会慢。

## 4. 镜头清单与校正策略（2026-09-09 实机实测）

> 实测（`pic_process scan F:\PS_Process`，全库 15,979 文件 / 8,603 ARW）结合解析本机全部 56 个 lensfun XML / 1,563 条 `<lens>` 条目 + 上游 master 交叉验证得出。

| 镜头 | ARW 张数 | Sony E 口 lensfun 条目 | 支持校正项 | 结论 |
|---|---|---|---|---|
| FE 24mm F1.4 GM | 2569 | ✅ `mil-sony.xml` | 畸变+色差+暗角 | 直接用 lensfun |
| E 18-135mm F3.5-5.6 OSS | 1731 | ✅ `mil-sony.xml` | 畸变+色差+暗角 | 直接用 lensfun |
| **Sigma 50mm F1.4 DG DN \| Art 023** | **1518** | ❌ **无条目** | — | **🔴 必须兜底（占 17.6%，最高优先级）** |
| Viltrox 23mm F1.4 E | 1397 | ✅ `misc.xml` | 畸变+色差+暗角 | 可用；**标定源自富士 X-T20**（cropfactor 1.53 vs E 口 1.534），失真/TCA 可靠、**暗角可能略偏**，建议抽样目视 |
| E 70-350mm F4.5-6.3 G OSS | 1231 | ✅ `mil-sony.xml` | 畸变+色差+暗角 | 直接用 lensfun |
| FE 24-70mm F2.8 GM II | 151 | ✅ `mil-sony.xml` | 畸变+色差；**暗角缺** | 用 lensfun，暗角另想办法（**一代 GM 的暗角系数不可套用二代**） |
| Sigma 70-200mm F2.8 DG DN OS \| Sports 023 | 4 | ❌ 无条目 | — | 需兜底（优先级极低） |
| FE 200-600mm F5.6-6.3 G OSS | 0（已购入未拍摄） | ✅ `mil-sony.xml` | 畸变+色差+暗角 | 已确认覆盖，拍到即可用 |

**关键背景**：
- darktable 的 lens correction 模块只走 **lensfun 数据库**，按 EXIF 自动匹配，匹配不到则“无 profile”（[官方文档](https://darktable-org.github.io/dtdocs/en/module-reference/processing-modules/lens-correction/)、[社区反馈](https://github.com/darktable-org/darktable/issues/11022)）；内嵌校正数据（embedded DNG corrections）仅 DNG 支持（[PR #12880](https://github.com/darktable-org/darktable/pull/12880)），**ARW 不适用**。
- 索尼 ARW 的 maker notes 里**内嵌镜头校正数据**（畸变/暗角/色差），RawTherapee 的 Lens/Geometry 可读取（[参考](https://photo.stackexchange.com/questions/114615/raw-therapee-lens-geometry-correction-sony-a6100/114621)）→ 可作**备用引擎**。
- 当时本机 lensfun 版本 **0.3.4**（随 darktable 5.6.1）。
- **对 Phase 1 无影响**：索尼 JPG 出厂即烘焙镜头校正，评分用 JPG 天然已校正。
- `18-135mm` 与 `23mm f/1.4` 在 `mil-fujifilm.xml` 里也有同名条目（`XF18-135mmF3.5-5.6R LM OIS WR`、`XF23mmF1.4 R`），**那是富士 X 口，不是命中**；两支镜头各自另有真正的 Sony E 口条目。
- **不要假定“命中 Sony E 口 = 三项校正齐全”**：`FE 24-70 GM II` 缺暗角；`FE 70-200mm f/2.8 GM OSS` 只有畸变。通用判定逻辑必须**逐条目解析 `<calibration>`**，不能只看是否命中。

**兜底链路（按优先级）**：darktable lensfun 命中 → RawTherapee 读索尼内嵌数据（同一张 ARW 换引擎出图）→ `lens_calibrate` 自校准 → 跳过（仅限畸变可忽略的镜头）。

> ⚠️ **`lensfun-update-data` 类的升级路径救不了**：当时已抓上游 master 验证，**两支适马（50/1.4 DG DN、70-200 DG DN OS）上游同样没有条目**，只能自建 profile 或跳过。真正“完全无 lensfun”的合计 **1,522 张 / 8,603 = 17.7%**。

## 5. P2-M1 侧车方案实测（2026-09-09）

> 结论来源：对 darktable 5.6.1 逐字段/逐偏移实测 + 源码核对（详见本地 `p2_notes/xmp_fields.md`、参考侧车 `p2_notes/reference/DSC00886.ARW.xmp`、机器可读模板 `xmp_template_ILCE-7M5_dt5.6.1.json`）。

### 5.1 侧车命名：两条命名各司其职，不冲突

| 文件名 | 谁读 | 读什么 |
|---|---|---|
| **`<原文件名含扩展名>.xmp`**（`DSC00886.ARW.xmp`） | **darktable 正牌侧车** | 全部，**含 `darktable:history`** |
| `<stem>.xmp`（`DSC00886.xmp`） | darktable 仅经 `dt_lightroom_import()` | **只有 rating / Label / 关键词；`darktable:history` 被静默忽略** |
| `<stem>.xmp` | **Lightroom / ACR（M7 决策）** | rating 等 |

→ Phase 1 保持写 `<stem>.xmp` 给 Lightroom。若实施 Phase 2 编排器，需另写 `<原名>.ARW.xmp`（含完整 history + `xmp:Rating` / `firstcut:*`，因为正牌侧车存在时 LR 风格那份不再被合并）。`darktable-cli` 自动发现同目录同名侧车，且 CLI 全程只读不写侧车。

### 5.2 四个“写错就静默失效”的点

- 顶层 `darktable:xmp_version` **合法范围只有 2..5**（缺失/0/1 → legacy 解析 → history 静默丢弃；≥6 → 不支持）。
- 每条 history 必须齐 `operation` + `params` + `modversion`，缺任一 → **整条丢弃**。
- `darktable:enabled` 默认是 **0（关闭）**，必须显式写 1。
- `darktable:history_end` = 实际应用条数。

### 5.3 完整栈模板 + 定点补丁（不可用“部分侧车”）

部分侧车实测不可用：只写 1 条 exposure 时 darktable 只补 9 条强制模块（**不补 sigmoid / color calibration**），结果 512px mean 128.1（无侧车默认 138.7）并随参数剧烈漂移（EV=2.2 时过曝 65.6%）。

→ 做法：用 `darktable-cli <copy.ARW> <out> --library <tmp>/library.db --core --configdir <tmp>/cfg` 导入一张该机型 ARW，从临时库 `history` 表抓**默认栈**（本机 ILCE-7M5 = 11 条、ILCE-6700 = 13 条，含 modversion 与原始 hex；6700 的 lens/denoiseprofile 自带，见 §7 ⑦ 重抓流程），之后只做三件事：**原地** patch exposure 的 EV、**原地替换** temperature 的系数、**patch-or-append** lens 与 denoiseprofile（栈里有则原地、无则追加，§7 ②）。实测完整栈原样回灌与“无侧车导出”逐像素几乎一致（Δmean 0.002/255）→ 模板法可靠。

### 5.4 四个模块的 `op_params` 结构（小端 hex，均无指针/变长）

| 模块 | modversion / 大小 | 关键字段 |
|---|---|---|
| `lens` | v10 / 356B | 偏移 332 `has_been_set=0` → 走 EXIF+lensfun **自动匹配**（`camera[]`/`lens[]` 留空）。默认栈里没有时需追加 |
| `exposure` | v7 / 28B | 偏移 8 = EV。**陷阱**：本机 ARW 机身曝光补偿 −1.3EV，默认 `comp_bias=1` 会补上 → ΔEV 要**原地读模板自带 EV 再加 Δ**（7M5 基线 0.7 / **6700 基线 1.2**，§7 ②），且**保持 `comp_bias=1`**。**不得硬编码 0.7** |
| `denoiseprofile` | v12 / 416B | `a[0]` 必须 **−1.0f** = 按相机+ISO 自动取 profile（**写 0 实测导出几乎全黑**）；偏移 8 = `strength`；`mode=4` 为 wavelets-auto |
| `temperature` | v4 / 20B | **没有 Kelvin/tint**，只有红绿蓝系数；`preset=2` 才用给定系数（`preset=0` as-shot 会被 EXIF 覆盖）；**必须原地替换**默认栈那条（追加会变两个实例叠加） |

### 5.5 版本强绑定与防御性校验

模板与 **darktable 版本**强绑定（modversion/params 长度随版本变），但 2026-09-18 实测 7M5 与 6700 默认栈共有模块**逐字节相同**，机型相关值只有 exposure 的 EV 基线（§7 ②）→ 按机身+版本存模板保留（防升级），不必再为颜色矩阵担心跨机型。Rust 侧用 `darktable-cli --version` 做模板键，并且**导出后必须校验非全黑**——版本或 params 长度写错时 darktable **不报错**，直接给黑图或未初始化内存（`modversion` 偏新 → params 是未初始化内存）。

当时待确认：`firstcut:*` 在 GUI「改图→保存侧车」后是否 100% 保留（源码上会保留）；自定义 `iop_order_version`；Kelvin/tint → 相机 RGB 系数的精确换算（`wb_presets.json` 无 ILCE-7M5）；降噪强度分级最优值需目视；LR 对 darktable 16bit TIFF 的兼容性。

## 6. P2-M2 编排器实测（2026-09-09）

> 结论来源：darktable-cli 5.6.1 Windows 下 27 组用例 + 并发压测 + 基准（详见本地 `p2_notes/cli_batch.md`）。

### 6.1 并发模型：一张一进程 + worker 池，默认并发 6

实测（12 张全分辨率 16bit TIFF）：P=4 → 27.4s，**P=6 → 24.0s（最优，2.0s/张）**，P=8 → 26.2s（退化），**P=12 → 81.2s（比串行还慢 2×，灾难）**。原因：单进程已能吃满 16 逻辑核，超订导致线程爆炸 + 换页（P=12 时单张 CPU 从 12s 涨到 48s，约 200 线程）。

→ `并发 = clamp(cores/3, 2, 8)`，并加**内存闸门**（全分辨率每张预留 1.2GB，2048px JPG 预留 0.6GB）。**不要用“一进程多图”**：位置参数传多个输入只有第一张被导出（其余静默忽略、exit 0）；只有重复 `--import` 才真多图，但牺牲逐图错误归因、内存线性增长且不更快。

### 6.2 成功判定：必须以文件系统为准，绝不能靠退出码

实测 3 张里 2 张成功 + 1 张损坏 → **退出码仍是 1**（整批聚合，无法区分全败/部分败）。

```text
运行前：create_dir_all(outdir)；记录 outdir 已有文件
spawn ：darktable-cli <输入正斜杠绝对路径> <输出目录正斜杠>/ --out-ext <ext> --apply-custom-presets 0
判定  ：exists(outdir/(stem+"."+ext)) && size > MIN_BYTES   # jpg ≥ 10KB，tif ≥ 1MB
```

日志兜底**只能匹配英文模块名前缀**（`[export_job] exported to`、`[imageio_storage_disk] could not export`、`[rawspeed] ... corrupt`）——运行期错误提示随 locale 变，匹配中文必失效。

### 6.3 三个必须做的前置动作（否则静默产出垃圾）

1. **路径全部 `\` → `/` 并在构造参数后断言不含 `\`**：反斜杠被**直接删除**而非转义，`F:\p2cli\out\` 会落盘到 `F:\p2cli\p2cliout\`，exit 0 且无警告。
2. **自己先 `create_dir_all` 输出目录**：目录不存在时 `$(FILE_NAME)` 不展开，写出字面名文件 `$(FILE_NAME).jpg` 且 **exit 0、日志照样写 exported**。
3. **加 `--apply-custom-presets 0`**：GUI 在跑时 CLI 默认 0.66s 内 `database lock ... aborting` exit 1；该选项是官方给的“multiple instances”开关。注意 `--library` **不能**绕过锁。

### 6.4 另外 7 个坑

- 加了 `--apply-custom-presets 0` 后 **`--style` 必然失败**（style 存数据库里）→ 渲染方案必须走预生成含 history 的 XMP，不能用 `--style`。
- **重跑不覆盖**：同目录已有产物时新文件叫 `_01`/`_02`，导致“按预期文件名判成功”的编排器第二次跑会误判全失败 → 每 worker 独立输出目录，或跑前清空，并按“本次新增文件”判定。
- 位置参数传第二张图被静默忽略（exit 0）；`--import DIR` **不递归**。
- **配置目录无法隔离**（`libdarktable.dll` 里没有 `DARKTABLE_CONFIGDIR`/`XDG_CONFIG_HOME`，“假 LOCALAPPDATA”无效）；CLI 每次运行都会重写 `darktablerc`（幂等，未观测到损坏）→ 当共享资源处理。
- **中文路径其实可用**：之前的“不可用”是 PowerShell 5.1 把 UTF-8 无 BOM 的 `.ps1` 按 GBK 解码造成的假象；Rust 用宽字符 API 无此问题。但反斜杠的 `--library` 路径会触发 `GLib-CRITICAL g_utf8_to_utf16` 断言。
- **不要用 `--library`**（它的语义是“从库读 history **而非** XMP 侧车”，指向没导入过这些照片的库 = 全部按默认导出）。
- `--verbose` 无帮助；无 `--bpp`（help 标 unsupported）、无 `--format`。

### 6.5 性能、资源与安全结论

| 输出 | 单张墙钟 | 峰值工作集 | 产物 |
|---|---|---|---|
| 全分辨率 16bit TIFF | **~8.1s** | 870MB–1.1GB | 45.5MB |
| 全分辨率 JPG | ~5.0s | ~1.05GB | 8.77MB |
| 2048px JPG | ~3.7s | ~0.36GB | 736KB |

内存是阶梯式：~70MB 解码 → ~360MB → **865MB 平台**。TIFF 已验证：4688×7032、16bit、3 通道、`Compression=8` (Adobe Deflate)、内嵌 ICC 9012B。**产物体积随机身/内容差异大**：6700 全幅样片实测 117–127MB（§7 ③）→ 磁盘预算按 **130MB/张**（8000 张 ≈ 1TB），45.5MB 只是下限参考。

**安全结论**：CLI **不写也不改 XMP 侧车**（前后 MD5 一致），源图无侧车也不新建 → 原库可安全只读使用。

**历史命令模板（2026-09-18 更新，含 16bit 强制）**：

```bat
darktable-cli.exe <正斜杠输入绝对路径> <已创建的正斜杠输出目录>/ --out-ext tif --apply-custom-presets 0 ^
  --core --conf plugins/imageio/format/tiff/bpp=16
```

> 勿用 `--style` / `--library` / `--bpp` / `--verbose`（`--bpp` 被 help 标 unsupported；位深只能经 `--core --conf` 覆盖，profile 默认 `bpp=8` 会静默出 8bit 图）。当时未确认项：`--apply-custom-presets 0` 在已有自定义预设时的成像影响；OpenCL 是否启用（`cltest` 跑 10 分钟未返回，按纯 CPU 对待）。

## 7. 动作 A 真图冒烟实录（2026-09-18，ILCE-6700）

> 样片从 `F:\PS_Process` 只读挑出并拷入本地 `p2_notes/_smoke/input/`（全库 15,979 文件重扫复核镜头普查，与 §4 一致）。样片：**DSC02906**（E 70-350 G，lensfun 命中路径）+ **DSC08051**（Sigma 50/1.4 Art，lensfun 无条目兜底路径）。
> **机身普查**：ILCE-6700 = 8,360 ARW（97%）、ILCE-7M5 = 239、ILCE-7RM5 = 4 → 模板必须按机身备，6700 是主力。

1. **EV 补丁生效（隔离实验，同图同模板只变 delta）**：

| delta-ev | 写入 EV（6700 基线 1.2） | JPG mean/255 |
|---|---|---|
| 0 | 1.2 | 131.0 |
| +0.5 | 1.7 | 154.6 |
| +3.0 | 4.2 | 233.1（近裁剪，符合 3 档预期） |

同 EV 下 16bit TIFF 与 JPG 像素对拍 5 点差 ≤2/255 → 侧车被读、EV 浮点补丁落地、管线非全黑。

2. **模板发现（修订 §5）**：7M5 与 6700 的默认栈**共有 11 个模块逐字节相同**（`rawprepare` / `demosaic` / `colorin` / `colorout` / `temperature` / `gamma` / `highlights` / `channelmixerrgb` / `flip` / `sigmoid` + `exposure` 的 black/percentile/comp 字段）。唯一机型相关值：**exposure 默认 EV 基线 7M5 = 0.7、6700 = 1.2**。且 6700 默认栈**自带** `lens`(v10, 356B) + `denoiseprofile`(v12, 416B)（共 13 条），7M5 不带（11 条）——6700 自带的两个 blob 与设计的 auto blob（`has_been_set=0` / `a[0]=-1.0f`）**逐字节一致**。→ 生成器逻辑定案为 **patch-or-append**，EV 补丁原地读模板 EV 加 delta（不得硬编码 0.7）。
3. **16bit TIFF 解锁**：profile 默认 `plugins/imageio/format/tiff/bpp=8`，强制 16bit 需 `--core --conf plugins/imageio/format/tiff/bpp=16`。实测 6700 全分辨率（6520×4176 = 27MP 全幅）16bit TIFF = **117–127MB/张** → 磁盘预算按 **130MB/张**。
4. **TIFF 压缩怪癖**：darktable 5.6.1 写的 TIFF `Compression` tag = 8（规范名义 = LZW），但 strip 流实际是 **zlib**（`78 9c` 魔数）。GDI+/.NET 读取正常；严格 LZW 阅读器兼容性未知，可调 `plugins/imageio/format/tiff/compress`。
5. **验收策略**：当时自写 Python TIFF 解码器在 strip→行映射上有 bug（不要再信任自解 TIFF 像素）。定案为每张图另导一份同 EV JPG 做 mean/峰值/过曝率交叉核对，TIFF 本体只验存在 + 大小 + `BPS tag=16`。
6. **暗角实测**：70-350（lensfun 路径）JPG 3×3 网格 corner/center = **0.947**、edge/center = **0.968**（未校正约 0.7）→ 镜头校正生效；Sigma 50（兜底路径）导出干净无畸变。
7. **默认栈重抓流程（新机身/darktable 升级时用）**：新目录放空 `library.db`（0 字节）→ `darktable-cli <图> <out>/ --out-ext jpg --apply-custom-presets 0 --library <dir>/library.db` → 查询 `select num,module,operation,op_params,enabled,blendop_params,blendop_version,multi_priority,multi_name,multi_name_hand_edited from history where imgid=1 order by num`。
8. **本地研究资产**（位于 gitignore 的 `p2_notes/_smoke/`）：`deliv/` 样片、`make_sidecar2.py`、`stack6700.json`、`tiff_probe.py` / `probe16.py` / `cross_cmp.py` / `check_sidecar.py`、`lib6700/`、`reference/xmp_template_ILCE-6700_dt5.6.1.json`。
9. **停止时仍未决项**：`firstcut:*` GUI 保存后保留性；OpenCL；Kelvin→RGB 换算；降噪强度分级目视；LR 对 16bit TIFF 的兼容性。

## 8. 历史里程碑与实施清单（已停止，不实施）

> 以下为已停止路线的原始清单，仅供追溯，**不代表当前待办**。

### 历史里程碑规划
- **P2-M1**：环境验证 —— 安装 darktable，darktable-cli 手动跑通一张 ARW 全流程（校正+降噪+导出 TIFF/JPG）
- **P2-M2**：Rust 编排器 —— 生成 XMP 侧车 + 调用 darktable-cli + 并发批处理 + 进度/日志
- **P2-M3**：联动 Phase 1 —— 曝光补偿值写入侧车、按评分阈值决定开发名单
- **P2-M4**：实测调优 —— 镜头跑一轮，调 lensfun 覆盖、降噪强度分级、输出验证

### 原始清单留档
- **P2-M0 环境与可行性**（已完成调研，结论见 §3–§7）
  - [x] 装 darktable 5.x（实装 5.6.1）
  - [x] `darktable-cli` 命令行跑通 ARW → TIFF/JPG
  - [x] `darktable-cltest` 确认 OpenCL/GPU 状态（10 分钟未返回 → 按纯 CPU 对待）
  - [x] 确认 neural restore CLI 不可用，改用 `rawdenoise` + `denoise (profiled)`
  - [x] 8 支镜头全库普查与 lensfun 覆盖实测
- **P2-M1 侧车模板与字段映射**（方案已冒烟定案，Rust 未开工）
  - [x] 确定写 `lens(auto)` / `exposure` / `denoiseprofile`
  - [x] 手工侧车验证读取与 EV 阶梯生效
  - [x] EV 偏移换算定案（原地读模板 EV 加 delta）
  - [ ] 降噪强度分级策略（已停止）
  - [ ] `src/sidecar.rs` Rust 移植（已停止）
- **P2-M2 / P2-M3 / P2-M4**（未开工，已停止）
  - [ ] `develop <目录>` 子命令与并发编排器
  - [ ] `--min-stars` / 断点续跑 / 失败重试 / 16bit TIFF + JPG 输出
  - [ ] 与 Lightroom 出图对比、逐镜头验证、百张耗时与万张磁盘规模基线
- **当时开放项**
  - [x] 输出格式：16bit TIFF 归档 + JPG（后随路线停止作废）
  - [ ] 是否保留 darktable `.xmp` / 白平衡策略 / 降噪分级依据 / 额外高分名单导出（已停止）
