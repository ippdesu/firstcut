# firstcut v1.1 — 修复合并版（AI 预处理通道布局 + 权重修正）

用 Rust 编写的本地照片初筛工具：扫描索尼相机 JPG+ARW 目录，五维评分（清晰度 / 曝光 / 噪点 / 构图 / 美学）+ 连拍去重，输出 CSV 报告与 Lightroom 兼容的 XMP 星级侧车。全程本地离线运行，照片不上传。

## v1.1 变更（相对 v1.0）

**🐛 修复：AI 预处理通道布局 bug**
- 人脸检测（SCRFD）、美学评分（CLIPIQA）、姿态检测（YOLOv8-pose）的预处理
  批量改写曾误将 RGB 交错数据按平面拆分，导致三模型输入通道错乱
  （实测美学分 15-25、人像 0 脸）
- 修复后与 v1.0 逐像素实现**输出完全一致**（逐张比对通过）

**⚖️ 权重修正**：默认权重 0.30/0.25/0.15/0.15/0.15（和 = 1.0）
- 曝光从 0.20 提升到 0.25（欠曝照片不再虚高），从清晰度挪 0.05 保持总和 1.0

**🧪 集成测试**：新增 6 项端到端测试；本地 testpic 缺失时自动跳过（克隆环境不再报错）

**🔧 工程改进**：二进制统一命名（`pic_process-*`）、base64 标准 crate 替代自写、版本号 1.0.0

## 功能

- **五维评分**（权重可配，`--config` 多场景配置）：
  - 清晰度：主体感知三层链路（SCRFD 人脸区域 → YOLOv8-pose 头部区域 → 中性兜底），大光圈浅景深照片不会被误判
  - 曝光：过曝/欠曝比例 + 亮度偏离目标（目标亮度可配，夜景/亮调环境自适应）
  - 噪点：暗部平滑块 P15 + ISO 容忍度
  - 构图：人脸/人体三分法位置 + 大小 + 多人降权
  - 美学：CLIPIQA（CLIP 底座大模型）
- **连拍去重**：时间聚类 + dHash 感知哈希 → 组内排序，`-k` 控制保留数
- **XMP 星级侧车**（`--xmp`）：Lightroom 直接可读；他人侧车不覆盖
- **SQLite 增量缓存**：重跑只处理新照片（秒级）
- **gallery 工具**：HTML 联系表（缩略图 + 分数）

## 使用

```bash
pic_process.exe config-template -o portrait.toml   # 生成评分配置模板
pic_process.exe score <照片目录> --xmp             # 评分 + XMP 星级
pic_process.exe score <照片目录> --config portrait.toml --cache portrait.sqlite
pic_process-gallery.exe report.csv -o gallery.html # HTML 联系表
```

## 模型准备（一次性，models/ 目录）

| 文件 | 来源 | 大小 |
|---|---|---|
| `clipiqa_model.onnx` + `.onnx.data` | hf-mirror.com/86Cao/IQA-ONNX-Models | ~153MB |
| `scrfd_10g_bnkps.onnx` | hf-mirror.com/RuteNL/SCRFD-face-detection-ONNX | 16.9MB |
| `yolov8n_pose.onnx` | hf-mirror.com/Xenova/yolov8n-pose | 13.5MB |

模型缺失时自动降级为纯像素评分；`--no-ai` 可显式跳过。

## 验证

- 12 单元测试 + 6 集成测试全过
- 119 张 33MP 真实照片冷跑 ~18s（16 核）；修复后 AI 输出与 v1.0 逐张一致
