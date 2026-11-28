//! 单次选片结果：CLI 与 review 共用的算法建议、人工覆盖和连拍候选。

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;

use crate::config::ScoreConfig;
use crate::decision::{self, CurationFlag, EffectiveBurst, EffectiveRating};
use crate::output::xmp;
use crate::scan::PhotoEntry;
use crate::score::{self, AnalysisResult};

/// 针对某个照片批次与配置计算的结果。key 均为扫描器提供的 pair_id。
pub struct SelectionResult {
    pub bursts: HashMap<String, EffectiveBurst>,
    pub ratings: HashMap<String, EffectiveRating>,
    pub flags: HashMap<String, CurationFlag>,
}

pub fn build(
    root: &Path,
    entries: &[PhotoEntry],
    analyzed: &HashMap<String, AnalysisResult>,
    cfg: &ScoreConfig,
    keep_override: Option<usize>,
) -> Result<SelectionResult> {
    build_in(&root.join(".firstcut"), root, entries, analyzed, cfg, keep_override)
}

pub fn build_in(
    state_dir: &Path,
    jpg_root: &Path,
    entries: &[PhotoEntry],
    analyzed: &HashMap<String, AnalysisResult>,
    cfg: &ScoreConfig,
    keep_override: Option<usize>,
) -> Result<SelectionResult> {
    let dedup = crate::config::effective_dedup(keep_override, cfg);
    let algo_bursts = score::analyze_photo_bursts(entries, analyzed, &dedup, &cfg.weights);
    let manual_bursts = decision::load_bursts_in(state_dir)?;
    let bursts = decision::effective_bursts_checked(jpg_root, entries, &algo_bursts, &manual_bursts)?;
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
    let manual = decision::load_ratings_in(state_dir)?;
    let ratings = decision::effective_ratings_checked(jpg_root, entries, &recommended, &manual)?;
    let manual_flags = decision::load_flag_map_in(state_dir)?;
    let flags = decision::effective_flags_checked(jpg_root, entries, &manual_flags)?;
    Ok(SelectionResult { bursts, ratings, flags })
}
