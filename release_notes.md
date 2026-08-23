# firstcut v1.0 — 索尼照片初筛评分工具

用 Rust 编写的本地照片初筛工具：扫描索尼相机 JPG+ARW 目录，五维评分（清晰度 / 曝光 / 噪点 / 构图 / 美学）+ 连拍去重，输出 CSV 报告与 Lightroom 兼容的 XMP 星级侧车。全程本地离线运行，照片不上传。

## 功能

- **五维评分**（权重可配）：
  - 清晰度：主体感知三层链路（SCRFD 人脸区域 → YOLOv8-pose 头部区域 → 中性兜底），大光圈浅景深照片不会被误判
  - 曝光：过曝/欠曝比例 + 亮度偏离目标（目标亮度可配，夜景/亮调环境自适应）
  - 噪点：暗部平滑块 P15 + ISO 容忍度
  - 构图：人脸/人体三分法位置 + 大小 + 多人降权
  - 美学：CLIPIQA（CLIP 底座大模型）
- **连拍去重**：时间聚类 + dHash 感知哈希 → 组内排序，`-k` 控制保留数
- **XMP 星级侧车**（`--xmp`）：Lightroom 直接可读；他人侧车不覆盖
- **SQLite 增量缓存**：重跑只处理新照片（秒级）
- **多场景配置**：`--config` 加载 TOML（人像/打鸟/夜景/飞机各存一份），`config-template` 生成模板
- **gallery 工具**：HTML 联系表（缩略图 + 分数，浏览器快速选片）

## 使用

```bash
# 生成评分配置模板（可选，改权重用）
pic_process.exe config-template -o portrait.toml

# 评分 + 写 XMP 星级（Lightroom 可读）
pic_process.exe score <照片目录> --xmp

# 多场景配置
pic_process.exe score <照片目录> --config portrait.toml --cache portrait.sqlite

# HTML 联系表（人工复核）
pic_process-gallery.exe report.csv -o gallery.html

# 调参：导出每张图的原始指标
pic_process-tune.exe <照片目录> -o metrics.csv
```

## 模型准备（一次性，`models/` 目录）

| 文件 | 来源 | 大小 |
|---|---|---|
| `clipiqa_model.onnx` + `.onnx.data` | hf-mirror.com/86Cao/IQA-ONNX-Models | ~153MB |
| `scrfd_10g_bnkps.onnx` | hf-mirror.com/RuteNL/SCRFD-face-detection-ONNX | 16.9MB |
| `yolov8n_pose.onnx` | hf-mirror.com/Xenova/yolov8n-pose | 13.5MB |

模型缺失时自动降级为纯像素评分；`--no-ai` 可显式跳过。

## 本版本内容

- **Phase 1 完整实现**：M0 扫描/EXIF → M1 像素指标 → M2 连拍去重 → M3 AI 五维评分（MUSIQ → CLIPIQA）→ M4 XMP+缓存 → M5 调参验证
- **用户反馈驱动的修复**：
  - 浅景深清晰度误判（主体感知三层链路：SCRFD 人脸区域 reblur → YOLOv8-pose 头部区域 reblur → 50 分中性下限）
  - 人脸漏检（YuNet → SCRFD 10g，119 张真实照片检出 23→75）
  - 小脸人像主体定位（SCRFD 漏检时回退 YOLOv8-pose 头部关键点）
  - 噪点暗部纹理污染（中位数 → P15 低百分位）
  - 多场景权重配置（`--config` / `config-template`）
- **测试覆盖**：12 单元测试（dedup 6 + composition 4 + xmp 2）+ 5 集成测试，端到端 pipeline 验证
- **性能**：119 张 33MP 真实照片冷跑 ~10.6s（16 核，含 CLIPIQA + SCRFD + 姿态），增量重跑秒级
