//! Connection manager on the welcome screen (child module of `app`), laid
//! Out' connection window:
//!
//! - toolbar: `+` (New Connection / New Group) and "Search for connection…"
//! - "Recent" (last used first, ⌘1–⌘5)
//! - groups (collapsible, renamable inline, can be empty) and ungrouped
//!   profiles; each row = round `Pg` badge · name · inline colored tag
//!   `(production)` · `host : database` underneath (+ `via ssh`)
//! - right-click a row: Connect · New ▸ · Edit… · Duplicate · Copy as URL ·
//!   Move to Group ▸ · Delete…; right-click a group: New Connection · Rename ·
//!   Delete Group (members become ungrouped).

use gpui_kit::component::input::InputEvent;
use gpui_kit::component::menu::PopupMenu;

use super::*;
use crate::dialog::ConnDialog;

impl TuskApp {
    /// Saved-profile indexes, most recently used first (never-used last).
    pub(super) fn recent_indexes(&self) -> Vec<usize> {
        let mut ix: Vec<usize> = (0..self.form.saved.len()).collect();
        ix.sort_by_key(|&i| std::cmp::Reverse(self.form.saved[i].last_used.unwrap_or(0)));
        ix
    }

    /// All group names: explicit (groups.json) ∪ referenced by profiles.
    fn folders(&self) -> Vec<String> {
        let mut f: Vec<String> = self.groups.clone();
        f.extend(self.form.saved.iter().filter_map(|c| c.folder.clone()));
        f.sort_by_key(|s| s.to_lowercase());
        f.dedup();
        f
    }

    fn persist_saved(&mut self, cx: &mut Context<Self>) {
        if let Err(e) = db::save_connections(&self.form.saved) {
            self.form.notice = Some((false, format!("Save failed: {e:#}")));
        }
        cx.notify();
    }

    fn persist_groups(&mut self, cx: &mut Context<Self>) {
        let _ = db::save_groups(&self.groups);
        cx.notify();
    }

    fn move_to_folder(&mut self, ix: usize, folder: Option<String>, cx: &mut Context<Self>) {
        if let Some(c) = self.form.saved.get_mut(ix) {
            c.folder = folder;
        }
        self.persist_saved(cx);
    }

    /// "New Group": add `New Group`, `New Group 2`… and edit its name inline.
    pub(super) fn new_group(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let existing = self.folders();
        let mut name = "New Group".to_string();
        let mut n = 2;
        while existing.contains(&name) {
            name = format!("New Group {n}");
            n += 1;
        }
        self.groups.push(name.clone());
        self.persist_groups(cx);
        self.start_group_rename(name, window, cx);
    }

    fn start_group_rename(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let input = cx.new(|cx| {
            let mut st = InputState::new(window, cx);
            st.set_value(name.clone(), window, cx);
            st
        });
        let sub = cx.subscribe_in(&input, window, |this, _, ev: &InputEvent, _, cx| match ev {
            InputEvent::PressEnter { .. } | InputEvent::Blur => this.commit_group_rename(cx),
            _ => {}
        });
        input.read(cx).focus_handle(cx).focus(window, cx);
        self.group_renaming = Some((name, input, sub));
        cx.notify();
    }

    fn commit_group_rename(&mut self, cx: &mut Context<Self>) {
        let Some((old, input, _sub)) = self.group_renaming.take() else {
            return;
        };
        let new = input.read(cx).value().trim().to_string();
        if new.is_empty() || new == old {
            cx.notify();
            return;
        }
        for g in &mut self.groups {
            if *g == old {
                *g = new.clone();
            }
        }
        if !self.groups.contains(&new) {
            self.groups.push(new.clone());
        }
        for c in &mut self.form.saved {
            if c.folder.as_deref() == Some(old.as_str()) {
                c.folder = Some(new.clone());
            }
        }
        if self.conn_pick == FolderPick::Group(old.clone()) {
            self.conn_pick = FolderPick::Group(new);
        }
        self.persist_groups(cx);
        self.persist_saved(cx);
    }

    /// Delete a group; its profiles move to the top level (nothing is lost).
    fn delete_group(&mut self, name: String, cx: &mut Context<Self>) {
        if self.conn_pick == FolderPick::Group(name.clone()) {
            self.conn_pick = FolderPick::All;
        }
        self.groups.retain(|g| *g != name);
        for c in &mut self.form.saved {
            if c.folder.as_deref() == Some(name.as_str()) {
                c.folder = None;
            }
        }
        self.persist_groups(cx);
        self.persist_saved(cx);
    }

    fn delete_connection(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self.form.saved.get(ix).map(|c| c.name.clone()) else {
            return;
        };
        let answer = window.prompt(
            PromptLevel::Critical,
            &format!("Delete “{name}”?"),
            Some("The saved profile and its Keychain passwords are removed."),
            &["Delete", "Cancel"],
            cx,
        );
        cx.spawn(async move |weak, cx: &mut AsyncApp| {
            if answer.await != Ok(0) {
                return;
            }
            let _ = weak.update(cx, |this: &mut TuskApp, cx| {
                if let Some(pos) = this.form.saved.iter().position(|c| c.name == name) {
                    this.form.saved.remove(pos);
                    db::delete_secrets(&name);
                    this.persist_saved(cx);
                }
            });
        })
        .detach();
    }

    fn matches_search(&self, ix: usize, q: &str) -> bool {
        if q.is_empty() {
            return true;
        }
        self.form.saved.get(ix).is_some_and(|c| {
            [&c.name, &c.host, &c.database]
                .iter()
                .any(|f| f.to_lowercase().contains(q))
        })
    }

    /// One connection row: name + inline colored `(tag)` · `host : database`
    /// underneath. No DB badge — Tusk only speaks PostgreSQL.
    pub(super) fn connection_row(
        &self,
        id: impl Into<ElementId>,
        ix: usize,
        hint: String,
        indent: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(conn) = self.form.saved.get(ix) else {
            return div().into_any_element();
        };
        let t = cx.theme();
        let (muted, fg) = (t.muted_foreground, t.foreground);
        let mut detail = format!("{} · {}", conn.host, conn.database);
        if let Some(s) = &conn.ssh {
            detail.push_str(&format!(" · ssh {}", s.host));
        }
        let folders = self.folders();
        div()
            .id(id.into())
            .cursor_pointer()
            .flex()
            .items_center()
            .gap_2()
            .w_full()
            .h(px(24.))
            .mt(px(1.))
            .pl(px(5.))
            .pr(px(6.))
            .when(indent, |this| this.pl(px(22.)))
            .rounded(crate::theme::RADIUS_SM)
            .hover(|this| this.bg(muted.opacity(0.08)))
            .child(
                Icon::new(IconName::Database)
                    .size(px(12.))
                    .text_color(muted),
            )
            .child(
                div()
                    .flex_none()
                    .text_sm()
                    .text_color(fg)
                    .child(conn.name.clone()),
            )
            .children(conn.tag.map(|tag| {
                div()
                    .flex_none()
                    .text_caption()
                    .text_color(rgb(tag.color()))
                    .child(tag.label())
            }))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_caption()
                    .font_family(crate::settings::ui_font())
                    .text_color(muted.opacity(0.55))
                    .child(detail),
            )
            .child(div().flex_none().text_sm().text_color(muted).child(hint))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.on_pick_saved(ix, true, window, cx);
            }))
            .context_menu(move |menu, window, cx| {
                let call = |f: fn(&mut TuskApp, usize, &mut Window, &mut Context<TuskApp>)| {
                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                        let view = cx.global::<TuskHandle>().0.clone();
                        view.update(cx, |this, cx| f(this, ix, window, cx));
                    }
                };
                let folders = folders.clone();
                let move_menu = PopupMenu::build(window, cx, move |mut m, _, _| {
                    m = m.item(PopupMenuItem::new("No Group").on_click(move |_, _, cx| {
                        let view = cx.global::<TuskHandle>().0.clone();
                        view.update(cx, |this, cx| this.move_to_folder(ix, None, cx));
                    }));
                    for f in &folders {
                        let go = f.clone();
                        m = m.item(PopupMenuItem::new(f.clone()).on_click(move |_, _, cx| {
                            let view = cx.global::<TuskHandle>().0.clone();
                            view.update(cx, |this, cx| {
                                this.move_to_folder(ix, Some(go.clone()), cx)
                            });
                        }));
                    }
                    m
                });
                let new_menu = PopupMenu::build(window, cx, |m, _, _| {
                    m.item(PopupMenuItem::new("Connection…").on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(NewConnection), cx);
                    }))
                    .item(PopupMenuItem::new("Group").on_click(|_, window, cx| {
                        let view = cx.global::<TuskHandle>().0.clone();
                        view.update(cx, |this, cx| this.new_group(window, cx));
                    }))
                });
                menu.item(
                    PopupMenuItem::new("Connect")
                        .on_click(call(|this, ix, w, cx| this.on_pick_saved(ix, true, w, cx))),
                )
                .separator()
                .item(PopupMenuItem::submenu("New", new_menu))
                .item(
                    PopupMenuItem::new("Edit…").on_click(call(|this, ix, _w, cx| {
                        if let Some(c) = this.form.saved.get(ix).cloned() {
                            ConnDialog::open_edit(c, false, cx);
                        }
                    })),
                )
                .item(
                    PopupMenuItem::new("Duplicate").on_click(call(|this, ix, _w, cx| {
                        if let Some(c) = this.form.saved.get(ix).cloned() {
                            ConnDialog::open_edit(c, true, cx);
                        }
                    })),
                )
                .separator()
                .item(
                    PopupMenuItem::new("Copy as URL").on_click(call(|this, ix, _w, cx| {
                        if let Some(c) = this.form.saved.get(ix) {
                            cx.write_to_clipboard(ClipboardItem::new_string(c.url()));
                        }
                    })),
                )
                .item(PopupMenuItem::submenu("Move to Group", move_menu))
                .separator()
                .item(crate::theme::danger_item(
                    "Delete…",
                    call(|this, ix, w, cx| this.delete_connection(ix, w, cx)),
                ))
            })
            .into_any_element()
    }

    pub(super) fn toggle_conn_manager(&mut self, cx: &mut Context<Self>) {
        self.conn_manager = !self.conn_manager;
        cx.notify();
    }

    fn pick_matches(&self, ix: usize, q: &str) -> bool {
        let Some(c) = self.form.saved.get(ix) else {
            return false;
        };
        // A search looks through every folder.
        if !q.is_empty() {
            return self.matches_search(ix, q);
        }
        match &self.conn_pick {
            FolderPick::All => true,
            FolderPick::Ungrouped => c.folder.is_none(),
            FolderPick::Group(g) => c.folder.as_deref() == Some(g.as_str()),
        }
    }

    /// Left rail entry: icon · name · count; groups rename inline and have a
    /// context menu (rename / delete / new connection).
    fn folder_item(
        &self,
        pick: FolderPick,
        icon: IconName,
        label: String,
        count: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = cx.theme();
        let (muted, fg, accent) = (t.muted_foreground, t.foreground, t.accent);
        let active = self.conn_pick == pick;
        let group = match &pick {
            FolderPick::Group(g) => Some(g.clone()),
            _ => None,
        };
        let editing = group.as_ref().and_then(|g| {
            self.group_renaming
                .as_ref()
                .filter(|(n, _, _)| n == g)
                .map(|(_, input, _)| input.clone())
        });
        let id = SharedString::from(format!("folder-{label}-{}", group.is_some()));
        let body: AnyElement = match editing {
            Some(input) => div()
                .flex_1()
                .child(Input::new(&input).small().h(px(20.)))
                .into_any_element(),
            None => div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_sm()
                .text_color(if active { fg } else { muted })
                .child(label)
                .into_any_element(),
        };
        let click_pick = pick.clone();
        let row = div()
            .id(id)
            .flex()
            .items_center()
            .gap_2()
            .h(px(26.))
            .px_2()
            .rounded(crate::theme::RADIUS_SM)
            .when(active, |this| this.bg(accent.opacity(0.14)))
            .when(!active, |this| {
                this.hover(|this| this.bg(muted.opacity(0.08)))
            })
            .child(
                Icon::new(icon)
                    .size(px(13.))
                    .text_color(if active { accent } else { muted }),
            )
            .child(body)
            .child(
                div()
                    .flex_none()
                    .text_caption()
                    .text_color(muted.opacity(0.6))
                    .child(count.to_string()),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.conn_pick = click_pick.clone();
                cx.notify();
            }));
        let Some(g) = group else {
            return row.into_any_element();
        };
        row.context_menu(move |menu, _, _| {
            let (r, d) = (g.clone(), g.clone());
            menu.item(
                PopupMenuItem::new("New Connection…").on_click(|_, window, cx| {
                    window.dispatch_action(Box::new(NewConnection), cx);
                }),
            )
            .separator()
            .item(
                PopupMenuItem::new("Rename Group").on_click(move |_, window, cx| {
                    let view = cx.global::<TuskHandle>().0.clone();
                    view.update(cx, |this, cx| {
                        this.start_group_rename(r.clone(), window, cx)
                    });
                }),
            )
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

    /// Saved connections as a sheet over the app (⌘⇧O), command-palette
    /// palette style: title bar with "+ New", full-width search, folders on
    /// the left, the picked folder's connections on the right.
    pub(super) fn render_conn_manager(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.theme();
        let (border, bg, fg, muted) = (t.border, t.popover, t.foreground, t.muted_foreground);
        let backdrop = crate::theme::backdrop(t);
        let q = self.conn_search.read(cx).value().trim().to_lowercase();

        let new_button = Button::new("conn-new")
            .ghost()
            .small()
            .icon(Icon::new(IconName::Plus).size(px(13.)))
            .label("New")
            .dropdown_menu(|menu, _, _| {
                menu.item(
                    PopupMenuItem::new("New Connection…").on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(NewConnection), cx);
                    }),
                )
                .item(PopupMenuItem::new("New Group").on_click(|_, window, cx| {
                    let view = cx.global::<TuskHandle>().0.clone();
                    view.update(cx, |this, cx| this.new_group(window, cx));
                }))
            });
        let header = div()
            .flex()
            .items_center()
            .justify_between()
            .h(px(40.))
            .pl_4()
            .pr_2()
            .border_b_1()
            .border_color(border)
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(fg)
                    .child("Connections"),
            )
            .child(new_button);
        let search = div()
            .px_2()
            .h(px(40.))
            .flex()
            .items_center()
            .border_b_1()
            .border_color(border)
            .child(
                Input::new(&self.conn_search)
                    .appearance(false)
                    .prefix(Icon::new(IconName::Search).size(px(14.)).text_color(muted)),
            );

        // ---- left: folders ----
        let saved = &self.form.saved;
        let count = |f: &dyn Fn(&SavedConnection) -> bool| saved.iter().filter(|c| f(c)).count();
        let mut rail = div()
            .w(px(180.))
            .flex_none()
            .flex()
            .flex_col()
            .gap_0p5()
            .p_2()
            .border_r_1()
            .border_color(border)
            .child(self.folder_item(
                FolderPick::All,
                IconName::Layers,
                "All Connections".into(),
                saved.len(),
                cx,
            ));
        let folders = self.folders();
        if !folders.is_empty() {
            rail = rail.child(
                div()
                    .px_2()
                    .pt_3()
                    .pb_1()
                    .text_caption()
                    .text_color(muted.opacity(0.7))
                    .child("GROUPS"),
            );
        }
        for folder in folders {
            let n = count(&|c| c.folder.as_deref() == Some(folder.as_str()));
            rail = rail.child(self.folder_item(
                FolderPick::Group(folder.clone()),
                IconName::Folder,
                folder,
                n,
                cx,
            ));
        }
        let loose = count(&|c| c.folder.is_none());
        if loose > 0 && loose < saved.len() {
            rail = rail.child(div().h(px(6.))).child(self.folder_item(
                FolderPick::Ungrouped,
                IconName::Inbox,
                "No Group".into(),
                loose,
                cx,
            ));
        }

        // ---- right: connections ----
        let rows: Vec<usize> = (0..saved.len())
            .filter(|&i| self.pick_matches(i, &q))
            .collect();
        let list: AnyElement =
            if rows.is_empty() {
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_sm()
                    .text_color(muted)
                    .child(if q.is_empty() {
                        "No connections here"
                    } else {
                        "No matches"
                    })
                    .into_any_element()
            } else {
                div()
                    .flex()
                    .flex_col()
                    .children(rows.into_iter().map(|ix| {
                        self.connection_row(("mgr-conn", ix), ix, String::new(), false, cx)
                    }))
                    .into_any_element()
            };
        let body = div().flex().h(px(340.)).child(rail).child(
            div()
                .id("conn-manager-scroll")
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .overflow_y_scroll()
                .p_2()
                .child(list),
        );
        let footer = div()
            .flex()
            .items_center()
            .justify_between()
            .h(px(30.))
            .px_4()
            .border_t_1()
            .border_color(border)
            .text_caption()
            .text_color(muted.opacity(0.7))
            .child("click to connect · right-click for more")
            .child("esc");

        div()
            .id("conn-manager")
            .absolute()
            .inset_0()
            .bg(backdrop)
            .flex()
            .justify_center()
            .items_start()
            .pt(px(72.))
            .cursor_pointer()
            .on_click(cx.listener(|this, _, _, cx| {
                this.conn_manager = false;
                cx.notify();
            }))
            .child(
                div()
                    .id("conn-manager-card")
                    .w(px(680.))
                    .flex()
                    .flex_col()
                    .rounded(crate::theme::RADIUS_LG)
                    .border_1()
                    .border_color(border)
                    .bg(bg)
                    .shadow_lg()
                    .overflow_hidden()
                    // Clicks inside the card don't close it.
                    .cursor_pointer()
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .child(header)
                    .child(search)
                    .child(body)
                    .child(footer),
            )
            .into_any_element()
    }
}

/// Which folder the connection manager shows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum FolderPick {
    #[default]
    All,
    Ungrouped,
    Group(String),
}
