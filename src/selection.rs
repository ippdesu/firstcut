//! 单次选片结果：CLI 与 review 共用的算法建议、人工覆盖和连拍候选。

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;

use crate::config::ScoreConfig;
use crate::decision::{self, EffectiveRating};
use crate::dedup::BurstInfo;
use crate::output::xmp;
use crate::scan::PhotoEntry;
use crate::score::{self, AnalysisResult};

/// 针对某个照片批次与配置计算的结果。key 均为扫描器提供的 pair_id。
pub struct SelectionResult {
    pub bursts: HashMap<String, BurstInfo>,
    pub ratings: HashMap<String, EffectiveRating>,
}

pub fn build(
    root: &Path,
    entries: &[PhotoEntry],
    analyzed: &HashMap<String, AnalysisResult>,
    cfg: &ScoreConfig,
    keep_override: Option<usize>,
) -> Result<SelectionResult> {
    let dedup = crate::config::effective_dedup(keep_override, cfg);
    let bursts = score::analyze_photo_bursts(entries, analyzed, &dedup, &cfg.weights);
    let totals: Vec<(String, f64)> = analyzed
        .iter()
        .map(|(key, result)| {
            (
                key.clone(),
                score::total_score(&result.scores, &cfg.weights),
            )
        })
        .collect();
    let recommended = xmp::assign_ratings(&totals, &cfg.metric);
    let manual = decision::load(root)?;
    let ratings = decision::effective_ratings(root, entries, &recommended, &manual)?;
    Ok(SelectionResult { bursts, ratings })
}
