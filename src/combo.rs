//! The drop-down list under an editing grid cell (data_type, is_nullable,
//! index_algorithm, is_unique): ten rows tall, scrolls with the wheel and
//! keeps the highlighted entry in view through its `ScrollHandle`.

use gpui_kit::component::table::{TableDelegate, TableState};
use gpui_kit::component::theme::ActiveTheme as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

const SHOWN: f32 = 10.;

/// `items` below the cell, `pick` highlighted; a click calls `on_pick`.
pub fn list<D: TableDelegate + 'static>(
    items: &[String],
    pick: usize,
    scroll: &ScrollHandle,
    width: f32,
    on_pick: impl Fn(&mut TableState<D>, String, &mut Context<TableState<D>>) + Clone + 'static,
    cx: &mut Context<TableState<D>>,
) -> AnyElement {
    let t = cx.theme();
    let (fg, muted, popover, border, active) = (
        t.foreground,
        t.muted_foreground,
        t.popover,
        t.border,
        t.tokens.table_active,
    );
    let rows = div()
        .id("combo-list")
        .max_h(px(SHOWN * crate::settings::row_h()))
        .overflow_y_scroll()
        .track_scroll(scroll)
        .children(items.iter().enumerate().map(|(i, name)| {
            let name_c = name.clone();
            let on_pick = on_pick.clone();
            div()
                .id(("combo-item", i))
                .mx_1()
                .px_2()
                .h(px(crate::settings::row_h()))
                .flex()
                .items_center()
                .rounded(crate::theme::RADIUS_SM)
                .text_size(px(crate::settings::table_text()))
                .font_family(crate::settings::table_font())
                .text_color(fg)
                .when(i == pick, |d| d.bg(active))
                .hover(|d| d.bg(muted.opacity(0.12)))
                .child(name.clone())
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |state, _, _, cx| {
                        cx.stop_propagation();
                        on_pick(state, name_c.clone(), cx);
                        cx.notify();
                    }),
                )
        }));
    let list = div()
        .absolute()
        .top_full()
        .left_0()
        .mt_1()
        .w(px(width))
        .py_1()
        .rounded(crate::theme::RADIUS_MD)
        .border_1()
        .border_color(border)
        .bg(popover)
        .shadow_lg()
        .occlude()
        .child(rows);
    deferred(list).with_priority(3).into_any_element()
}

/// `options` containing `query` (case-insensitive), prefix matches first;
/// all of them for an empty query.
pub fn filter(options: &[&str], query: &str) -> Vec<String> {
    let q = query.trim().to_lowercase();
    let (mut starts, mut contains) = (Vec::new(), Vec::new());
    for o in options {
        let l = o.to_lowercase();
        if q.is_empty() || l.starts_with(&q) {
            starts.push(o.to_string());
        } else if l.contains(&q) {
            contains.push(o.to_string());
        }
    }
    starts.extend(contains);
    starts
}

#[cfg(test)]
mod tests {
    #[test]
    fn filter_prefix_first() {
        let o = ["BTREE", "HASH", "GIST", "GIN", "BRIN", "SPGIST"];
        assert_eq!(super::filter(&o, ""), o.map(String::from).to_vec());
        assert_eq!(super::filter(&o, "gi"), ["GIST", "GIN", "SPGIST"]);
        assert_eq!(super::filter(&["YES", "NO"], "n"), ["NO"]);
    }
}
