# 历史与过期评审归档（`docs/history/`）

> 本目录统一归档已过期的代码评审记录、早期里程碑、历史决策过程以及已停止的技术路线研究。
> **当前有效的产品边界、架构与实现约定请以 [`docs/DESIGN.md`](../DESIGN.md) 为准。**

## 归档目录

| 文件 | 内容 | 说明 |
|---|---|---|
| [`REVIEWS.md`](REVIEWS.md) | 历次代码评审（第 1–4 轮、GLM 评审、Astra 可靠性评审）与 v1.3.0 / v1.4.1 维护审计记录 | 原 `REVIEW.md`、`ASTRA_REVIEW.md` 及 `docs/HISTORY.md` §2–§3 |
| [`MILESTONES_AND_DECISIONS.md`](MILESTONES_AND_DECISIONS.md) | 早期里程碑（M0–M9 / M-UI1）、模型选型证据、设计决策表（M6–M9）与研究参考 / UI 选型 | 原 `M5_REVIEW.md` 及 `docs/HISTORY.md` §4、§6、§7 |
| [`DARKTABLE_PHASE2_RESEARCH.md`](DARKTABLE_PHASE2_RESEARCH.md) | 已停止的 Phase 2（darktable 批量 RAW 开发）研究、镜头普查、侧车实测与并发基准 | 原 `P2_M0.md` 及 `docs/HISTORY.md` §5（2026-09-18 停止，不列入当前待办） |

## 注意事项

- 本目录文档解释**过去为什么这样做**；文中的旧代码行号、历史测试数量、旧默认值、阶段编号（M0–M9 / P2-M*）与未勾选清单**仅适用于对应历史版本**，不是当前实现规范或待办列表。
- 已移除或合并的早期评审文件原文均保留在 Git 历史中，可按需查阅：
  - `git show fad69a1:REVIEW.md`（原多轮综合评审）
  - `git show fad69a1:ASTRA_REVIEW.md`（原可靠性与数据模型评审）
  - `git show fad69a1:DESIGN.md`（合并前的旧版设计文档）
  - `git show fad69a1:release_notes.md`（早期完整版版本说明）
  - `git show 0592f98~1:M5_REVIEW.md` / `git show 0592f98~1:P2_M0.md`（更早阶段记录）
  - `git show c5555f2:docs/HISTORY.md`（拆分前的单文件历史档案）

## 后续记录规则

1. **当前约定**只在 [`docs/DESIGN.md`](../DESIGN.md) 维护，**用户可见变化**只在 [`docs/RELEASE_NOTES.md`](../RELEASE_NOTES.md) 维护。
2. 新的阶段性评审或审计结论统一追加到 [`REVIEWS.md`](REVIEWS.md)（写明日期、代码基线、问题、证据、处理结果与未验证边界），不再在仓库根目录按模型或里程碑新建散落的评审文档。
3. 若评审中发现仍然有效且需要跟进的问题，应同步更新 [`docs/DESIGN.md`](../DESIGN.md#9-后续工作与未验收项)。
