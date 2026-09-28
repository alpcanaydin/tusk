//! Column widths: fitted to the content (header + the first loaded rows)
//! unless the user resized a column — then that width is remembered per
//! table in `column_widths.json` and wins from then on.

use std::collections::BTreeMap;

use gpui_kit::{Pixels, px};

/// Width that fits `header` and `values` in the table font (mono-ish
/// estimate: ~0.62 em per char), within 56–460 px.
pub fn auto_width<'a>(header: &str, values: impl Iterator<Item = &'a str>) -> Pixels {
    let longest = values
        .map(|v| v.lines().next().unwrap_or_default().chars().count().min(64))
        .max()
        .unwrap_or(0);
    // Header text + sort arrow.
    let chars = longest.max(header.chars().count() + 3) as f32;
    let em = crate::settings::table_text();
    px((chars * em * 0.62 + 26.).clamp(56., 460.))
}

type WidthFile = BTreeMap<String, BTreeMap<String, f32>>;

fn path() -> std::path::PathBuf {
    crate::db::connections_path().with_file_name("column_widths.json")
}

fn read() -> WidthFile {
    std::fs::read_to_string(path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Saved widths of one table (`connection/database/schema.table`).
pub fn saved(key: &str) -> BTreeMap<String, f32> {
    read().remove(key).unwrap_or_default()
}

/// Remember the widths the user dragged to (column name → px).
pub fn save(key: &str, widths: BTreeMap<String, f32>) {
    let mut all = read();
    all.entry(key.to_string()).or_default().extend(widths);
    if let Ok(text) = serde_json::to_string_pretty(&all) {
        let _ = std::fs::write(path(), text);
    }
}

#[cfg(test)]
mod tests {
    use super::auto_width;

    #[test]
    fn fits_content_within_bounds() {
        let narrow = auto_width("id", ["1", "22"].into_iter());
        let wide = auto_width("email", ["someone.long@example.com"].into_iter());
        assert!(narrow < wide);
        assert!(f32::from(narrow) >= 56.);
        let huge = "x".repeat(500);
        assert!(f32::from(auto_width("t", [huge.as_str()].into_iter())) <= 460.);
    }
}
