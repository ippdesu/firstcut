# firstcut 文档索引

本目录集中维护 firstcut 的现行架构设计、版本说明与历史归档文档。

## 现行文档

| 文档 | 职责 |
|---|---|
| [`../README.md`](../README.md) | 项目首页：快速上手、Windows 启动器 / CLI / Docker 用法、模型准备、目录结构与测试入口 |
| [`DESIGN.md`](DESIGN.md) | **唯一现行设计与实现约定**：产品边界、四层数据模型、目录/缓存/配置语义、五维评分与连拍算法、Web UI、安全模型与后续计划 |
| [`RELEASE_NOTES.md`](RELEASE_NOTES.md) | 面向用户的版本变更说明（v1.1 – v1.4.2 及 Unreleased 开发进度） |
| [`../SECURITY.md`](../SECURITY.md) | 安全支持版本范围与私密漏洞报告渠道 |

## 历史与过期评审归档（[`history/`](history/README.md)）

以下文档仅作背景追溯，**不代表现行架构或待办清单**：

| 文档 | 归档内容 |
|---|---|
| [`history/README.md`](history/README.md) | 归档说明、Git 历史找回命令与后续评审追加规则 |
| [`history/REVIEWS.md`](history/REVIEWS.md) | 历次代码评审记录（第 1–4 轮、GLM / Astra 评审）与 v1.3.0 / v1.4.1 维护审计 |
| [`history/MILESTONES_AND_DECISIONS.md`](history/MILESTONES_AND_DECISIONS.md) | 早期里程碑（M0–M9 / M-UI1）、模型选型证据、M6–M9 决策表与 UI 选型背景 |
| [`history/DARKTABLE_PHASE2_RESEARCH.md`](history/DARKTABLE_PHASE2_RESEARCH.md) | 已停止的 Phase 2 darktable 批量 RAW 开发研究档案（2026-09-18 停止） |

## 文档维护规范

- **单一真相源**：架构或行为变更同步更新 [`DESIGN.md`](DESIGN.md)，用法变更同步更新 [`../README.md`](../README.md)，发行变更同步更新 [`RELEASE_NOTES.md`](RELEASE_NOTES.md)。
- **评审统一归档**：阶段性代码评审或安全审计统一追加到 [`history/REVIEWS.md`](history/REVIEWS.md)，不再在仓库根目录散落新建独立评审文件。
- **隐私与本地文件**：私人照片、本地测试集、模型权重、访问凭据与本地对话记忆（如 `memory.md`、`p2_notes/`、`review/`、`.zcode/`）通过 `.gitignore` 排除，不进入公开文档与版本库。
