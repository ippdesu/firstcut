//! CSV 报告输出（M0）

use std::path::Path;

use anyhow::Result;

use crate::scan::PhotoEntry;

/// 将照片索引写入 CSV（带表头）
pub fn write_csv(path: &Path, entries: &[PhotoEntry]) -> Result<()> {
    let mut wtr = csv::Writer::from_path(path)?;
    for e in entries {
        wtr.serialize(e)?;
    }
    wtr.flush()?;
    Ok(())
}
