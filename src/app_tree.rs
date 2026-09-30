//! "Connections tree" sidebar layout (child module of `app`): every saved
//! connection by group, like the welcome list, with the connected one
//! expanded to its schema's objects (the same header / list the "Objects"
//! layout shows). One window still holds one connection: picking another
//! row switches this window to it.

use gpui_kit::component::menu::PopupMenu;

use super::*;

impl TuskApp {
    /// Unsaved edits anywhere in this window: sidebar drops / renames plus
    /// every tab's grid, structure and result changes.
    fn pending_total(&self, cx: &App) -> usize {
        self.pending_drops.len()
            + self.pending_renames.len()
            + (0..self.tabs.len())
                .map(|ix| self.tab_pending(ix, cx))
                .sum::<usize>()
    }

    /// Switch this window to a saved connection, asking first when that
    /// would throw away unsaved changes.
    pub(super) fn switch_connection_guarded(
        &mut self,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(name) = self.form.saved.get(ix).map(|c| c.name.clone()) else {
            return;
        };
        if name == self.active_name {
            return;
        }
        let n = self.pending_total(cx);
        if n == 0 {
            self.switch_connection(ix, window, cx);
            return;
        }
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Switch to “{name}”?"),
            Some(&format!(
                "{n} unsaved change{} will be discarded.",
                if n == 1 { "" } else { "s" }
            )),
            &["Switch", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |weak, cx| {
            if answer.await != Ok(0) {
                return;
            }
            let _ = weak.update_in(cx, |this: &mut TuskApp, window, cx| {
                // The list may have changed while the prompt was up.
                if let Some(ix) = this.form.saved.iter().position(|c| c.name == name) {
                    this.switch_connection(ix, window, cx);
                }
            });
        })
        .detach();
    }

    /// "Connect" on a saved connection: from the welcome screen connect
    /// right away, from a workspace switch this window over.
    pub(super) fn open_saved(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.screen == AppScreen::Workspace {
            self.switch_connection_guarded(ix, window, cx);
        } else {
            self.on_pick_saved(ix, true, window, cx);
        }
    }

    /// One connection line: engine logo (tinted by its status color, as on
    /// the welcome list) · name · colored tag; the connected one is marked
    /// and carries a chevron.
    fn tree_connection_row(
        &self,
        ix: usize,
        folders: &[String],
        indent: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(conn) = self.form.saved.get(ix) else {
            return div().into_any_element();
        };
        let t = cx.theme();
        let (muted, fg, accent) = (t.muted_foreground, t.foreground, t.accent);
        let active = conn.name == self.active_name;
        let connecting = self.form.busy && self.form.selected == Some(ix);
        let icon = match crate::icons::engine_logo(conn.engine) {
            Some(bytes) => Icon::default().data(bytes),
            None => Icon::new(IconName::Database),
        };
        let tint: gpui::Hsla = conn.status_rgb().map_or(muted, |c| rgb(c).into());
        let folders = folders.to_vec();
        div()
            .id(("tree-conn", ix))
            .flex()
            .items_center()
            .gap_1p5()
            .pl(px(if indent { 16. } else { 2. }))
            .pr_2()
            .h(px(crate::settings::row_h()))
            .rounded(px(4.))
            .when(active, |this| this.bg(muted.opacity(0.12)))
            .hover(|this| this.bg(muted.opacity(0.08)))
            .child(div().w(px(12.)).flex_none().when(active, |this| {
                this.child(
                    Icon::new(IconName::ChevronDown)
                        .size(px(11.))
                        .text_color(muted),
                )
            }))
            .child(icon.size(px(13.)).text_color(tint))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(px(crate::settings::ui_text()))
                            .font_family(crate::settings::ui_font())
                            .text_color(if active { fg } else { muted })
                            .when(active, |this| this.font_weight(FontWeight::MEDIUM))
                            .child(conn.name.clone()),
                    )
                    .children(conn.tag.map(|tag| {
                        div()
                            .flex_none()
                            .text_caption()
                            .text_color(rgb(tag.color()))
                            .child(tag.label())
                    })),
            )
            .when(connecting, |this| {
                this.child(
                    gpui_kit::component::spinner::Spinner::new()
                        .xsmall()
                        .color(muted),
                )
            })
            .when(active && !connecting, |this| {
                this.child(div().size(px(6.)).flex_none().rounded_full().bg(accent))
            })
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_saved(ix, window, cx);
            }))
            .context_menu(move |menu, window, cx| {
                Self::connection_menu(ix, folders.clone(), menu, window, cx)
            })
            .into_any_element()
    }

    /// A group line: chevron · folder · name · count. Click folds it (saved
    /// in settings); right-click offers what the connection manager does.
    fn tree_group_row(
        &self,
        name: &str,
        count: usize,
        open: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let toggle = name.to_string();
        let menu_name = name.to_string();
        div()
            .id(SharedString::from(format!("tree-group-{name}")))
            .flex()
            .items_center()
            .gap_1p5()
            .pl(px(2.))
            .pr_2()
            .h(px(crate::settings::row_h()))
            .rounded(px(4.))
            .hover(|this| this.bg(muted.opacity(0.08)))
            .child(
                div().w(px(12.)).flex_none().child(
                    Icon::new(if open {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    })
                    .size(px(11.))
                    .text_color(muted),
                ),
            )
            .child(
                Icon::new(if open {
                    IconName::FolderOpen
                } else {
                    IconName::Folder
                })
                .size(px(13.))
                .text_color(muted),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(crate::settings::ui_text()))
                    .font_family(crate::settings::ui_font())
                    .text_color(muted)
                    .child(name.to_string()),
            )
            .child(
                div()
                    .flex_none()
                    .text_caption()
                    .text_color(muted)
                    .child(count.to_string()),
            )
            .on_click(move |_, _, cx| crate::settings::toggle_group_collapsed(cx, &toggle))
            .context_menu(move |menu: PopupMenu, _, _| {
                let d = menu_name.clone();
                menu.item(
                    PopupMenuItem::new("New Connection…").on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(NewConnection), cx);
                    }),
                )
                .item(
                    PopupMenuItem::new("Manage Connections…").on_click(|_, _, cx| {
                        let view = cx.global::<TuskHandle>().0.clone();
                        view.update(cx, |this, cx| this.toggle_conn_manager(cx));
                    }),
                )
                .separator()
                .item(crate::theme::danger_item(
                    "Delete Group",
                    move |_, _, cx| {
                        let view = cx.global::<TuskHandle>().0.clone();
                        view.update(cx, |this, cx| this.delete_group(d.clone(), cx));
                    },
                ))
            })
            .into_any_element()
    }

    /// The connections-tree sidebar. `objects_header` / `objects_list` are
    /// the "Objects" layout's panel header and list, nested under the
    /// connected row.
    pub(super) fn render_conn_tree(
        &self,
        objects_header: AnyElement,
        objects_list: AnyElement,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = cx.theme();
        let (muted, border, background) = (t.muted_foreground, t.border, t.background);
        let q = self.conn_search.read(cx).value().trim().to_lowercase();
        let saved = &self.form.saved;
        let folders = self.folders();
        let shown: Vec<usize> = (0..saved.len())
            .filter(|&i| self.matches_search(i, &q))
            .collect();

        let mut objects = Some(
            div()
                .flex()
                .flex_col()
                .pl(px(14.))
                .pb_1()
                .child(objects_header)
                .child(div().px_2().flex().flex_col().child(objects_list)),
        );

        // Rows in order: each group then its members, then the ungrouped.
        enum Line {
            Group(String, usize, bool),
            Conn(usize, bool),
        }
        let mut lines = Vec::new();
        for folder in &folders {
            let members: Vec<usize> = shown
                .iter()
                .copied()
                .filter(|&i| saved[i].folder.as_deref() == Some(folder.as_str()))
                .collect();
            if members.is_empty() && !q.is_empty() {
                continue;
            }
            // A search keeps a group open; otherwise the user's fold wins
            // (connecting unfolds the connection's group once, see
            // `connected_with`, but it can be folded again).
            let open = !q.is_empty() || !crate::settings::group_collapsed(folder);
            lines.push(Line::Group(folder.clone(), members.len(), open));
            if open {
                lines.extend(members.into_iter().map(|ix| Line::Conn(ix, true)));
            }
        }
        lines.extend(
            shown
                .iter()
                .filter(|&&ix| saved[ix].folder.is_none())
                .map(|&ix| Line::Conn(ix, false)),
        );

        let mut list = div().flex().flex_col();
        for line in lines {
            match line {
                Line::Group(name, n, open) => {
                    list = list.child(self.tree_group_row(&name, n, open, cx));
                }
                Line::Conn(ix, indent) => {
                    list = list.child(self.tree_connection_row(ix, &folders, indent, cx));
                    if saved[ix].name == self.active_name
                        && let Some(o) = objects.take()
                    {
                        list = list.child(o.when(indent, |o| o.pl(px(28.))));
                    }
                }
            }
        }
        // Connected without a saved profile (or it was renamed / deleted):
        // still show where the objects belong.
        if let Some(o) = objects.take()
            && !self.active_name.is_empty()
        {
            list = list
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1p5()
                        .pl(px(2.))
                        .pr_2()
                        .h(px(crate::settings::row_h()))
                        .rounded(px(4.))
                        .bg(muted.opacity(0.12))
                        .child(
                            div().w(px(12.)).flex_none().child(
                                Icon::new(IconName::ChevronDown)
                                    .size(px(11.))
                                    .text_color(muted),
                            ),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_size(px(crate::settings::ui_text()))
                                .font_family(crate::settings::ui_font())
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(cx.theme().foreground)
                                .child(self.active_name.clone()),
                        )
                        .child(
                            div()
                                .size(px(6.))
                                .flex_none()
                                .rounded_full()
                                .bg(cx.theme().accent),
                        ),
                )
                .child(o);
        }
        if shown.is_empty() && !q.is_empty() {
            list = list.child(
                div()
                    .px_2()
                    .py_1()
                    .text_caption()
                    .text_color(muted)
                    .child("No matching connections."),
            );
        }

        let search = div().px_2().pt_2().pb_1().child(
            Input::new(&self.conn_search)
                .xsmall()
                .prefix(Icon::new(IconName::Search).size(px(12.)).text_color(muted))
                .font_family(crate::settings::ui_font()),
        );
        let footer = div()
            .id("tree-new-connection")
            .flex()
            .flex_none()
            .items_center()
            .gap_1p5()
            .px_3()
            .h(px(crate::settings::bar_h()))
            .border_t_1()
            .border_color(border)
            .text_size(px(crate::settings::ui_text()))
            .font_family(crate::settings::ui_font())
            .text_color(muted)
            .hover(|this| this.bg(muted.opacity(0.08)))
            .child(Icon::new(IconName::Plus).size(px(12.)))
            .child("New Connection")
            .on_click(|_, window, cx| window.dispatch_action(Box::new(NewConnection), cx));

        div()
            .id("sidebar")
            .key_context("Sidebar")
            .track_focus(&self.sidebar_focus)
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(background)
            .child(search)
            .child(
                div()
                    .id("sidebar-tree")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_1()
                    .flex()
                    .flex_col()
                    .child(list),
            )
            .child(footer)
            .into_any_element()
    }
}
