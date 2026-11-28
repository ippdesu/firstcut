//! CSV 报告输出（M0）

use std::borrow::Cow;
use std::path::Path;

use anyhow::Result;

use crate::scan::PhotoEntry;

/// 将照片索引写入 CSV（带表头）
pub fn write_csv(path: &Path, entries: &[PhotoEntry]) -> Result<()> {
    write(crate::storage::FileRef::explicit(path, true)?, entries, false)?;
    Ok(())
}

pub fn write_csv_in(root: &Path, path: &Path, entries: &[PhotoEntry], spreadsheet: bool) -> Result<()> {
    write(crate::storage::FileRef::at(root, path, true)?, entries, spreadsheet)?;
    Ok(())
}

fn write(file: crate::storage::FileRef, entries: &[PhotoEntry], spreadsheet: bool) -> std::io::Result<()> {
    file.replace(|output| {
        let mut writer = csv::WriterBuilder::new()
            .quote_style(if spreadsheet { csv::QuoteStyle::Always } else { csv::QuoteStyle::Necessary })
            .from_writer(output);
        for entry in entries {
            if spreadsheet {
                let mut entry = entry.clone();
                for text in [&mut entry.path, &mut entry.filename, &mut entry.extension,
                    &mut entry.date_time_original, &mut entry.camera_make, &mut entry.camera_model,
                    &mut entry.lens_model, &mut entry.iso, &mut entry.f_number,
                    &mut entry.shutter_speed, &mut entry.focal_length] {
                    if let Cow::Owned(protected) = spreadsheet_text(text) {
                        *text = protected;
                    }
                }
                writer.serialize(entry)?;
            } else {
                writer.serialize(entry)?;
            }
        }
        writer.flush()
    })
}

/// Prefix a text marker before characters that spreadsheet programs can
/// interpret as a formula. Keep this separate from the machine-readable CSV.
pub fn spreadsheet_text(value: &str) -> Cow<'_, str> {
    let first = value.trim_start_matches(|c: char| c.is_whitespace() || c == '\u{feff}').chars().next();
    if value.starts_with(['\t', '\r', '\n'])
        || first.is_some_and(|c| matches!(c, '=' | '+' | '-' | '@' | '＝' | '＋' | '－' | '＠')) {
        Cow::Owned(format!("'{value}"))
    } else {
        Cow::Borrowed(value)
    }
}

/// Remove the leading spreadsheet text marker added by [`spreadsheet_text`].
pub fn unescape_spreadsheet_text(value: &str) -> &str {
    match value.strip_prefix('\'') {
        Some(rest) if matches!(spreadsheet_text(rest), Cow::Owned(_)) => rest,
        _ => value,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spreadsheet_csv_protects_text_and_raw_mode_preserves_it() {
        let album = tempfile::tempdir().unwrap();
        let path = album.path().join("report.csv");
        let entry = PhotoEntry {
            filename: "-example.jpg".into(),
            camera_model: "  ＋ 示例相机".into(),
            lens_model: "50 mm".into(),
            suggested_ev: "-0.33".into(),
            ..Default::default()
        };
        write_csv_in(album.path(), &path, std::slice::from_ref(&entry), true).unwrap();
        let row: PhotoEntry = csv::Reader::from_path(&path).unwrap().deserialize().next().unwrap().unwrap();
        assert_eq!(row.filename, "'-example.jpg");
        assert_eq!(unescape_spreadsheet_text(&row.filename), "-example.jpg");
        assert_eq!(row.camera_model, "'  ＋ 示例相机");
        assert_eq!(unescape_spreadsheet_text(&row.camera_model), "  ＋ 示例相机");
        assert_eq!(unescape_spreadsheet_text("'normal"), "'normal");
        assert_eq!(row.lens_model, entry.lens_model);
        assert_eq!(row.suggested_ev, entry.suggested_ev);

        write_csv_in(album.path(), &path, std::slice::from_ref(&entry), false).unwrap();
        let row: PhotoEntry = csv::Reader::from_path(&path).unwrap().deserialize().next().unwrap().unwrap();
        assert_eq!(row.filename, entry.filename);
        assert_eq!(row.camera_model, entry.camera_model);
    }

    #[test]
    fn legacy_csv_without_curation_flag_column_deserializes_with_default() {
        let legacy_csv = concat!(
            "path,filename,extension,is_raw,has_pair,date_time_original,camera_make,camera_model,",
            "lens_model,iso,f_number,shutter_speed,focal_length,sharpness_score,exposure_score,",
            "noise_score,composition_score,aesthetic_score,total_score,suggested_ev,stars,",
            "rating_source,faces,analysis_ok,analysis_mode,burst_group,burst_size,burst_rank,",
            "burst_keep,burst_pose\n",
            "a.jpg,a.jpg,jpg,false,false,,,,,,,,,,,,,,,,,,,,,,,,,\n"
        );
        let row: PhotoEntry = csv::Reader::from_reader(legacy_csv.as_bytes())
            .deserialize()
            .next()
            .unwrap()
            .unwrap();
        assert_eq!(row.filename, "a.jpg");
        assert_eq!(row.curation_flag, "");
    }
}
