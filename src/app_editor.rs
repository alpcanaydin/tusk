//! SQL editor commands: line / block comment toggles, case transforms and
//! the text size (grid + editor) up / down.

use gpui_kit::component::input::EditorState;

use super::*;

/// Upper / lower case, or capitalize each word.
#[derive(Clone, Copy)]
pub enum CaseTransform {
    Upper,
    Lower,
    Capitalize,
}

/// `-- ` on / off for each line of `block` (all commented → uncomment).
fn toggle_lines(block: &str) -> String {
    let lines: Vec<&str> = block.split('\n').collect();
    let commented = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .all(|l| l.trim_start().starts_with("--"));
    lines
        .iter()
        .map(|l| {
            if l.trim().is_empty() {
                return l.to_string();
            }
            let indent = l.len() - l.trim_start().len();
            let (lead, rest) = l.split_at(indent);
            if commented {
                let rest = rest.strip_prefix("--").unwrap_or(rest);
                format!("{lead}{}", rest.strip_prefix(' ').unwrap_or(rest))
            } else {
                format!("{lead}-- {rest}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn transform(text: &str, how: CaseTransform) -> String {
    match how {
        CaseTransform::Upper => text.to_uppercase(),
        CaseTransform::Lower => text.to_lowercase(),
        CaseTransform::Capitalize => {
            let mut out = String::with_capacity(text.len());
            let mut start = true;
            for c in text.chars() {
                if c.is_alphanumeric() {
                    if start {
                        out.extend(c.to_uppercase());
                    } else {
                        out.extend(c.to_lowercase());
                    }
                    start = false;
                } else {
                    out.push(c);
                    start = true;
                }
            }
            out
        }
    }
}

impl TuskApp {
    fn active_sql_editor(&self) -> Option<Entity<EditorState>> {
        match self.active_tab.and_then(|ix| self.tabs.get(ix)) {
            Some(WorkspaceTab::Sql(t)) => Some(t.editor.clone()),
            _ => None,
        }
    }

    /// ⌘/: comment / uncomment the lines of the selection (or the cursor's).
    pub(super) fn toggle_line_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.active_sql_editor() else {
            return;
        };
        editor.update(cx, |st, cx| {
            let text = st.text().to_string();
            let sel = st.selected_range();
            let start = text[..sel.start.min(text.len())]
                .rfind('\n')
                .map_or(0, |i| i + 1);
            // A selection ending at a line start leaves that line alone.
            let sel_end = if sel.end > sel.start && text[..sel.end].ends_with('\n') {
                sel.end - 1
            } else {
                sel.end
            };
            let end = text[sel_end.min(text.len())..]
                .find('\n')
                .map_or(text.len(), |i| sel_end + i);
            let new = toggle_lines(&text[start..end]);
            st.set_selected_range(start..end, cx);
            st.replace(new.clone(), window, cx);
            st.set_selected_range(start..start + new.len(), cx);
        });
    }

    /// ⌥⌘/: wrap the selection in `/* */` (or unwrap it).
    pub(super) fn toggle_block_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.active_sql_editor() else {
            return;
        };
        editor.update(cx, |st, cx| {
            let sel = st.selected_range();
            let text = st.selected_text().to_string();
            let trimmed = text.trim();
            let new = if trimmed.starts_with("/*") && trimmed.ends_with("*/") && trimmed.len() >= 4
            {
                trimmed[2..trimmed.len() - 2].trim().to_string()
            } else {
                format!("/* {text} */")
            };
            st.replace(new.clone(), window, cx);
            st.set_selected_range(sel.start..sel.start + new.len(), cx);
        });
    }

    /// Edit ▸ Transformations on the selection.
    pub(super) fn transform_selection(
        &mut self,
        how: CaseTransform,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(editor) = self.active_sql_editor() else {
            return;
        };
        let empty = editor.read(cx).selected_range().is_empty();
        if empty {
            self.toast_info("Select some text first.");
            cx.notify();
            return;
        }
        editor.update(cx, |st, cx| {
            let sel = st.selected_range();
            let new = transform(&st.selected_text().to_string(), how);
            st.replace(new.clone(), window, cx);
            st.set_selected_range(sel.start..sel.start + new.len(), cx);
        });
    }

    /// ⌘C / ⇧⌘C in the data grid.
    pub(super) fn grid_copy(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut copied = None;
        self.with_active_grid(window, cx, |st, _, _| {
            copied = match st.selected_range() {
                Some((from, to)) => Some(st.delegate().copy_range(from, to)),
                None => st
                    .delegate()
                    .copy_text(st.selected_cell(), st.selected_row()),
            };
        });
        if let Some(text) = copied {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    /// ⇧⌘V in the data grid: paste into the cells from the selected one.
    pub(super) fn grid_paste(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) else {
            return;
        };
        let mut n = 0;
        self.with_active_grid(window, cx, |st, _, cx| {
            // Into a selected range, paste starts at its top-left cell.
            let start = st
                .selected_range()
                .map(|(from, _)| from)
                .or(st.selected_cell());
            if let Some((r, c)) = start {
                n = st.delegate_mut().paste_text(r, c, &text);
                cx.notify();
            }
        });
        if n > 0 {
            self.toast_info(format!("Pasted {n} cell{}", if n == 1 { "" } else { "s" }));
        }
    }

    /// ⌘= / ⌘- / ⌘0: grid + editor text size (the UI keeps its size).
    pub(super) fn step_font_size(&mut self, step: f32, cx: &mut Context<Self>) {
        crate::settings::update(cx, |p| {
            p.table_font_size = if step == 0. {
                13.
            } else {
                (p.table_font_size + step).clamp(9., 28.)
            };
        });
        let size = crate::settings::get().table_font_size;
        self.toast_info(format!("Text size {size}"));
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::{CaseTransform, toggle_lines, transform};

    #[test]
    fn line_comments_toggle_both_ways() {
        let on = toggle_lines("SELECT 1\n  FROM t");
        assert_eq!(on, "-- SELECT 1\n  -- FROM t");
        assert_eq!(toggle_lines(&on), "SELECT 1\n  FROM t");
        // Mixed → comment everything.
        assert_eq!(toggle_lines("-- a\nb"), "-- -- a\n-- b");
    }

    #[test]
    fn case_transforms() {
        assert_eq!(transform("select id", CaseTransform::Upper), "SELECT ID");
        assert_eq!(transform("SELECT Id", CaseTransform::Lower), "select id");
        assert_eq!(
            transform("user_name AND x", CaseTransform::Capitalize),
            "User_Name And X"
        );
    }
}
