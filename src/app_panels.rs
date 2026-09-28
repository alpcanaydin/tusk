//! History (a sidebar panel), Console and Problems (the panel under the
//! tabs) and their status-bar buttons.

use std::time::Duration;

use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};

use super::*;
use crate::console::{self, HistoryItem, Source};
use std::rc::Rc;

/// Console auto-scroll: the list's handle, and "go to the newest line" on
/// the next render (the line count is only known there).
#[derive(Default)]
pub struct ConsoleScroll {
    pub handle: gpui_kit::UniformListScrollHandle,
    pub stick: std::cell::Cell<bool>,
}

impl ConsoleScroll {
    pub fn scroll_to_bottom(&self) {
        self.stick.set(true);
    }
}

pub struct ConsoleCache {
    key: (u64, Option<Source>),
    lines: Rc<Vec<console::LogLine>>,
}

pub struct HistoryCache {
    key: (u64, String, String),
    items: Rc<Vec<HistoryItem>>,
}

impl BottomPanel {
    /// Same glyphs as the status-bar buttons.
    pub fn icon(self) -> Icon {
        match self {
            BottomPanel::Console => {
                Icon::default().data(include_bytes!("../assets/icons/ui/console.svg"))
            }
            BottomPanel::History => {
                Icon::default().data(include_bytes!("../assets/icons/ui/history.svg"))
            }
            BottomPanel::Problems => Icon::new(IconName::TriangleAlert),
        }
    }
}

/// Slide in / out duration of the bottom panel (`TUSK_SLIDE_MS` overrides,
/// for slowing it down while checking the motion).
fn slide_ms() -> u64 {
    static MS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *MS.get_or_init(|| {
        std::env::var("TUSK_SLIDE_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(180)
    })
}

/// What the panel under the tabs shows.
#[derive(Clone, Copy, PartialEq)]
pub enum BottomPanel {
    Console,
    History,
    Problems,
}

fn with_app(cx: &mut App, f: impl FnOnce(&mut TuskApp, &mut Context<TuskApp>)) {
    let view = cx.global::<TuskHandle>().0.clone();
    view.update(cx, f);
}

/// One line for lists: whitespace runs collapsed.
fn one_line(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}

impl TuskApp {
    /// Repaint the Console / History when statements are logged (they are
    /// recorded off the UI thread).
    pub(super) fn watch_console(cx: &mut Context<Self>) {
        cx.spawn(async move |weak, cx: &mut AsyncApp| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                let seq = console::seq();
                let alive = weak.update(cx, |this: &mut TuskApp, cx| {
                    if seq == this.console_seen {
                        return;
                    }
                    this.console_seen = seq;
                    let console_open = this.bottom_panel == Some(BottomPanel::Console);
                    if console_open {
                        this.console_scroll.scroll_to_bottom();
                    }
                    if console_open || this.bottom_panel == Some(BottomPanel::History) {
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    fn history_connection(&self) -> String {
        self.active_conn
            .as_ref()
            .map(|(c, _)| c.name.clone())
            .unwrap_or_default()
    }

    /// A history entry in a new query tab; `run` executes it right away.
    pub(super) fn open_history(
        &mut self,
        sql: String,
        run: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = if sql.trim_end().ends_with(';') {
            sql
        } else {
            format!("{};", sql.trim_end())
        };
        self.open_sql_tab_with(Some(text), window, cx);
        if run && let Some(ix) = self.active_tab {
            self.run_sql_in_tab(ix, crate::sql::RunScope::All, cx);
        }
    }

    /// Open / switch / close (the open one) — opening and closing slide.
    pub(super) fn toggle_bottom_panel(&mut self, panel: BottomPanel, cx: &mut Context<Self>) {
        match self.bottom_panel {
            Some(p) if p == panel && !self.bottom_closing => self.close_bottom_panel(cx),
            Some(_) if !self.bottom_closing => self.bottom_panel = Some(panel),
            _ => {
                self.bottom_panel = Some(panel);
                self.bottom_closing = false;
                self.bottom_anim += 1;
            }
        }
        if self.bottom_panel == Some(BottomPanel::Console) {
            self.console_scroll.scroll_to_bottom();
        }
        cx.notify();
    }

    pub(super) fn close_bottom_panel(&mut self, cx: &mut Context<Self>) {
        if self.bottom_panel.is_none() || self.bottom_closing {
            return;
        }
        self.bottom_closing = true;
        self.bottom_anim += 1;
        let anim = self.bottom_anim;
        cx.spawn(async move |weak, cx: &mut AsyncApp| {
            cx.background_executor()
                .timer(Duration::from_millis(slide_ms() + 20))
                .await;
            let _ = weak.update(cx, |this: &mut TuskApp, cx| {
                // Reopened meanwhile? Then keep it.
                if this.bottom_closing && this.bottom_anim == anim {
                    this.bottom_panel = None;
                    this.bottom_closing = false;
                    cx.notify();
                }
            });
        })
        .detach();
        cx.notify();
    }

    // ---------- History ----------

    /// Every query run on this connection, newest first: time, duration,
    /// database, SQL. Double-click opens it in a new tab, ▶ runs it there.
    /// A virtual list over a cached, filtered copy of the history.
    fn render_history_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.theme();
        let (muted, fg, red) = (t.muted_foreground, t.foreground, t.red);
        let needle = self.history_search.read(cx).value().to_lowercase();
        let conn = self.history_connection();
        let key = (console::seq(), needle.clone(), conn.clone());
        let items = {
            let mut cache = self.history_cache.borrow_mut();
            match cache.as_ref().filter(|c| c.key == key) {
                Some(c) => c.items.clone(),
                None => {
                    let items: Rc<Vec<HistoryItem>> = Rc::new(
                        console::history(&conn)
                            .into_iter()
                            .filter(|i| needle.is_empty() || i.sql.to_lowercase().contains(&needle))
                            .collect(),
                    );
                    *cache = Some(HistoryCache {
                        key,
                        items: items.clone(),
                    });
                    items
                }
            }
        };
        if items.is_empty() {
            return div()
                .px_3()
                .py_2()
                .text_xs()
                .text_color(muted)
                .child(if needle.is_empty() {
                    "Queries you run on this connection appear here."
                } else {
                    "No matching queries."
                })
                .into_any_element();
        }
        // One time column width for the whole list (today = time only).
        let today_date = chrono::Local::now().date_naive();
        let today = items.iter().all(|i| i.at.date_naive() == today_date);
        let (size, font) = (
            crate::settings::table_text() - 1.,
            crate::settings::table_font(),
        );
        uniform_list("history-list", items.len(), move |range, _, _| {
            range
                .map(|i| {
                    let item = &items[i];
                    let when = if item.at.date_naive() == today_date {
                        item.at.format("%H:%M:%S").to_string()
                    } else {
                        item.at.format("%b %-d %H:%M").to_string()
                    };
                    let (sql, sql_run, sql_menu, at) = (
                        item.sql.clone(),
                        item.sql.clone(),
                        item.sql.clone(),
                        item.at,
                    );
                    div()
                        .id(("history-row", i))
                        .cursor_pointer()
                        .group("history-row")
                        .w_full()
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_3()
                        .h(px(24.))
                        .text_size(px(size))
                        .font_family(font.clone())
                        .hover(|d| d.bg(muted.opacity(0.06)))
                        // Failed runs: the time in red.
                        .child(
                            div()
                                .flex_none()
                                .w(px(if today { 64. } else { 104. }))
                                .text_color(if item.ok { muted.opacity(0.6) } else { red })
                                .child(when),
                        )
                        .child(
                            div()
                                .flex_none()
                                .w(px(52.))
                                .text_color(muted.opacity(0.6))
                                .child(format!("{} ms", item.ms)),
                        )
                        .child(
                            div()
                                .flex_none()
                                .w(px(84.))
                                .truncate()
                                .text_color(muted.opacity(0.6))
                                .child(item.database.clone()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_color(fg)
                                .child(one_line(&item.sql)),
                        )
                        .child(
                            div()
                                .id(("history-run", i))
                                .cursor_pointer()
                                .flex_none()
                                .w(px(20.))
                                .h(px(20.))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(4.))
                                .text_color(muted)
                                .invisible()
                                .group_hover("history-row", |d| d.visible())
                                .hover(|d| d.text_color(fg).bg(muted.opacity(0.12)))
                                .child(Icon::new(IconName::Play).size(px(11.)))
                                .tooltip(|window, cx| {
                                    gpui_kit::component::tooltip::Tooltip::new("Run in New Tab")
                                        .build(window, cx)
                                })
                                .on_click(move |_, window, cx| {
                                    cx.stop_propagation();
                                    let s = sql_run.clone();
                                    with_app(cx, |app, cx| app.open_history(s, true, window, cx));
                                }),
                        )
                        .on_click(move |ev: &ClickEvent, window, cx| {
                            if ev.click_count() >= 2 {
                                let s = sql.clone();
                                with_app(cx, |app, cx| app.open_history(s, false, window, cx));
                            }
                        })
                        .context_menu(move |menu, _, _| {
                            Self::history_menu(sql_menu.clone(), at, menu)
                        })
                })
                .collect::<Vec<_>>()
        })
        .size_full()
        .py_1()
        .into_any_element()
    }

    fn history_menu(
        sql: String,
        at: chrono::DateTime<chrono::Local>,
        menu: PopupMenu,
    ) -> PopupMenu {
        let (s1, s2, s3, s4) = (sql.clone(), sql.clone(), sql.clone(), sql);
        menu.item(
            PopupMenuItem::new("Open in New Tab").on_click(move |_, window, cx| {
                let s = s1.clone();
                with_app(cx, |app, cx| app.open_history(s, false, window, cx));
            }),
        )
        .item(
            PopupMenuItem::new("Run in New Tab").on_click(move |_, window, cx| {
                let s = s2.clone();
                with_app(cx, |app, cx| app.open_history(s, true, window, cx));
            }),
        )
        .item(PopupMenuItem::new("Copy").on_click(move |_, _, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(s3.clone()));
        }))
        .separator()
        .item(
            PopupMenuItem::new("Remove from History").on_click(move |_, _, cx| {
                console::remove_history(at, &s4);
                with_app(cx, |_, cx| cx.notify());
            }),
        )
    }

    // ---------- Bottom panel ----------

    pub(super) fn render_bottom_panel(
        &self,
        panel: BottomPanel,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = cx.theme().clone();
        let (muted, fg, border) = (t.muted_foreground, t.foreground, t.border);
        let tab = |id: &'static str, label: &'static str, which: BottomPanel| {
            let on = panel == which;
            div()
                .id(id)
                .px_2()
                .h(px(24.))
                .flex()
                .items_center()
                .gap_1p5()
                .rounded(px(4.))
                .text_xs()
                .text_color(if on { fg } else { muted })
                .when(on, |d| d.bg(muted.opacity(0.14)))
                .hover(|d| d.text_color(fg))
                .child(which.icon().size(px(12.)))
                .child(label)
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.bottom_panel = Some(which);
                    cx.notify();
                }))
        };
        let tools: AnyElement = match panel {
            BottomPanel::Console => {
                let chip = |id: &'static str, label: &'static str, f: Option<Source>| {
                    let on = self.console_filter == f;
                    div()
                        .id(id)
                        .px_2()
                        .h(px(24.))
                        .flex()
                        .items_center()
                        .rounded(px(4.))
                        .text_xs()
                        .text_color(if on { fg } else { muted })
                        .when(on, |d| d.bg(muted.opacity(0.12)))
                        .hover(|d| d.text_color(fg))
                        .child(label)
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.console_filter = f;
                            this.console_scroll.scroll_to_bottom();
                            cx.notify();
                        }))
                };
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(chip("console-all", "All", None))
                    .child(chip("console-data", "Data", Some(Source::Data)))
                    .child(chip("console-meta", "Meta", Some(Source::Meta)))
                    .child(div().w(px(1.)).h(px(14.)).mx_1().bg(border))
                    .child(
                        Button::new("console-clear")
                            .label("Clear")
                            .small()
                            .outline()
                            .on_click(cx.listener(|_, _, _, cx| {
                                console::clear();
                                cx.notify();
                            })),
                    )
                    .into_any_element()
            }
            BottomPanel::History => div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div().w(px(240.)).child(
                        Input::new(&self.history_search)
                            .small()
                            .prefix(Icon::new(IconName::Search).size(px(12.)).text_color(muted))
                            .font_family(crate::settings::ui_font()),
                    ),
                )
                .child(
                    Button::new("history-clear")
                        .label("Clear")
                        .small()
                        .outline()
                        .on_click(cx.listener(|this, _, _, cx| {
                            console::clear_history(&this.history_connection());
                            cx.notify();
                        })),
                )
                .into_any_element(),
            BottomPanel::Problems => div().into_any_element(),
        };
        let header = div()
            .h(px(38.))
            .flex_none()
            .flex()
            .items_center()
            .gap_1()
            .px_2()
            .border_b_1()
            .border_color(border)
            .child(tab("panel-tab-console", "Console", BottomPanel::Console))
            .child(tab("panel-tab-history", "History", BottomPanel::History))
            .child(tab("panel-tab-problems", "Problems", BottomPanel::Problems))
            .child(div().flex_1())
            .child(tools)
            .child(
                div()
                    .id("panel-close")
                    .ml_1()
                    .w(px(20.))
                    .h(px(20.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(4.))
                    .text_color(muted)
                    .hover(|d| d.bg(muted.opacity(0.12)).text_color(fg))
                    .child(Icon::new(IconName::Close).size(px(12.)))
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| this.close_bottom_panel(cx))),
            );
        let body = match panel {
            BottomPanel::Console => self.render_console_lines(cx),
            BottomPanel::History => self.render_history_list(cx),
            BottomPanel::Problems => self.render_problems(cx),
        };
        let (h, closing) = (self.bottom_h, self.bottom_closing);
        // Floats over the tab content (nothing underneath moves).
        div()
            .absolute()
            .bottom_0()
            .left_0()
            .right_0()
            .overflow_hidden()
            .h(px(self.bottom_h))
            .shadow_lg()
            // Clicks and scrolls stay in the panel, not the grid below.
            .occlude()
            // It floats over the sidebar split line; draw that edge itself.
            .border_l_1()
            // The panel occludes the root, so a resize drag over it is
            // tracked here as well (dragging down, to shrink).
            .on_mouse_move(cx.listener(|this, e: &MouseMoveEvent, window, cx| {
                if let Some((y0, h0)) = this.bottom_drag {
                    let max = (f32::from(window.viewport_size().height) - 200.).max(120.);
                    this.bottom_h = (h0 - (f32::from(e.position.y) - y0)).clamp(90., max);
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, _| this.bottom_drag = None),
            )
            .flex()
            .flex_col()
            .border_t_1()
            .border_color(border)
            .bg(t.background)
            // Drag the top edge to resize.
            .child(
                div()
                    .id("bottom-panel-edge")
                    .absolute()
                    .top_0()
                    .left_0()
                    .right_0()
                    .h(px(5.))
                    .cursor_row_resize()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, e: &MouseDownEvent, _, _| {
                            this.bottom_drag = Some((f32::from(e.position.y), this.bottom_h));
                        }),
                    ),
            )
            .child(header)
            .child(div().flex_1().min_h_0().child(body))
            .with_animation(
                ("bottom-panel-slide", self.bottom_anim),
                Animation::new(Duration::from_millis(slide_ms())).with_easing(ease_out_quint()),
                move |d, delta| {
                    let f = if closing { 1. - delta } else { delta };
                    d.h(px(h * f))
                },
            )
            .into_any_element()
    }

    /// Every statement sent, one line each (a failed one ends in its error,
    /// in red; right-click copies the whole text). A virtual list over a
    /// cached, filtered copy of the log.
    fn render_console_lines(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.theme();
        let (muted, fg, red) = (t.muted_foreground, t.foreground, t.red);
        let key = (console::seq(), self.console_filter);
        let lines = {
            let mut cache = self.console_cache.borrow_mut();
            match cache.as_ref().filter(|c| c.key == key) {
                Some(c) => c.lines.clone(),
                None => {
                    let lines: Rc<Vec<console::LogLine>> = Rc::new(
                        console::lines()
                            .into_iter()
                            .filter(|l| self.console_filter.is_none_or(|f| l.source == f))
                            .collect(),
                    );
                    *cache = Some(ConsoleCache {
                        key,
                        lines: lines.clone(),
                    });
                    lines
                }
            }
        };
        if lines.is_empty() {
            return div()
                .px_3()
                .py_2()
                .text_xs()
                .text_color(muted)
                .child("Every statement Tusk sends shows up here.")
                .into_any_element();
        }
        if self.console_scroll.stick.replace(false) {
            self.console_scroll
                .handle
                .scroll_to_item(lines.len() - 1, gpui_kit::ScrollStrategy::Bottom);
        }
        let (size, font) = (
            crate::settings::table_text() - 1.,
            crate::settings::table_font(),
        );
        uniform_list("console-lines", lines.len(), move |range, _, _| {
            range
                .map(|i| {
                    let l = &lines[i];
                    let meta = l.source == Source::Meta;
                    let (copy, open) = (l.sql.clone(), l.sql.clone());
                    let full = match &l.error {
                        Some(e) => format!("{}\n-- {e}", l.sql),
                        None => l.sql.clone(),
                    };
                    div()
                        .id(("console-line", i))
                        .w_full()
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_3()
                        .h(px(22.))
                        .text_size(px(size))
                        .font_family(font.clone())
                        .hover(|d| d.bg(muted.opacity(0.06)))
                        .child(
                            div()
                                .flex_none()
                                .text_color(muted.opacity(0.6))
                                .child(l.at.format("%H:%M:%S%.3f").to_string()),
                        )
                        .child(
                            div()
                                .flex_none()
                                .w(px(52.))
                                .text_color(muted.opacity(0.6))
                                .child(format!("{} ms", l.ms)),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .gap_2()
                                .overflow_hidden()
                                .child(
                                    div()
                                        .flex_shrink(1.)
                                        .min_w_0()
                                        .truncate()
                                        .text_color(if meta { muted } else { fg })
                                        .child(one_line(&l.sql)),
                                )
                                .children(l.error.as_ref().map(|e| {
                                    div()
                                        .flex_shrink_0()
                                        .max_w(px(480.))
                                        .truncate()
                                        .text_color(red)
                                        .child(one_line(e))
                                })),
                        )
                        .context_menu(move |menu, _, _| {
                            let (a, b, c) = (copy.clone(), open.clone(), full.clone());
                            menu.item(PopupMenuItem::new("Copy").on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(a.clone()));
                            }))
                            .item(PopupMenuItem::new("Copy with Error").on_click(
                                move |_, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(c.clone()));
                                },
                            ))
                            .item(
                                PopupMenuItem::new("Open in New Tab").on_click(
                                    move |_, window, cx| {
                                        let s = b.clone();
                                        with_app(cx, |app, cx| {
                                            app.open_history(s, false, window, cx)
                                        });
                                    },
                                ),
                            )
                        })
                })
                .collect::<Vec<_>>()
        })
        .track_scroll(&self.console_scroll.handle)
        .size_full()
        .py_1()
        .into_any_element()
    }

    // ---------- Problems ----------

    /// (errors, warnings) across the open SQL tabs.
    pub(super) fn problem_counts(&self) -> (usize, usize) {
        use lsp_types::DiagnosticSeverity as S;
        let mut n = (0, 0);
        for (_, diags) in &self.problems {
            for d in diags {
                match d.severity {
                    Some(S::ERROR) | None => n.0 += 1,
                    _ => n.1 += 1,
                }
            }
        }
        n
    }

    fn render_problems(&self, cx: &mut Context<Self>) -> AnyElement {
        use lsp_types::DiagnosticSeverity as S;
        let t = cx.theme();
        let (muted, fg, red, yellow) = (t.muted_foreground, t.foreground, t.red, t.yellow);
        let mut col = div().flex().flex_col().py_1();
        let mut any = false;
        for (tab_ix, tab) in self.tabs.iter().enumerate() {
            let WorkspaceTab::Sql(s) = tab else { continue };
            let Some(uri) = s.doc.as_ref().map(|d| d.uri.clone()) else {
                continue;
            };
            let Some((_, diags)) = self.problems.iter().find(|(u, _)| *u == uri) else {
                continue;
            };
            any = true;
            col = col.child(
                div()
                    .px_3()
                    .pt_1()
                    .pb(px(2.))
                    .flex()
                    .items_center()
                    .gap_2()
                    .text_xs()
                    .text_color(fg)
                    .child(DbIcon::Sql.icon().size(px(12.)))
                    .child(s.title.clone())
                    .child(div().text_color(muted).child(diags.len().to_string())),
            );
            for (i, d) in diags.iter().enumerate() {
                let error = matches!(d.severity, Some(S::ERROR) | None);
                let pos = d.range.start;
                col = col.child(
                    div()
                        .id(SharedString::from(format!("problem-{tab_ix}-{i}")))
                        .cursor_pointer()
                        .flex()
                        .items_start()
                        .gap_2()
                        .pl(px(30.))
                        .pr_3()
                        .py(px(3.))
                        .text_xs()
                        .hover(|d| d.bg(muted.opacity(0.08)))
                        .child(
                            Icon::new(if error {
                                IconName::CircleX
                            } else {
                                IconName::TriangleAlert
                            })
                            .size(px(12.))
                            .text_color(if error {
                                red
                            } else {
                                yellow
                            }),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_color(fg)
                                .child(one_line(&d.message)),
                        )
                        .child(div().flex_none().text_color(muted).child(format!(
                            "{}:{}",
                            pos.line + 1,
                            pos.character + 1
                        )))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.jump_to_problem(tab_ix, pos, window, cx)
                        })),
                );
            }
        }
        if !any {
            col = col.child(
                div()
                    .px_3()
                    .py_1()
                    .text_xs()
                    .text_color(muted)
                    .child("No problems in the open queries."),
            );
        }
        div()
            .id("problems-scroll")
            .size_full()
            .overflow_y_scroll()
            .child(col)
            .into_any_element()
    }

    fn jump_to_problem(
        &mut self,
        tab_ix: usize,
        pos: lsp_types::Position,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.activate_tab(tab_ix, cx);
        if let Some(WorkspaceTab::Sql(s)) = self.tabs.get(tab_ix) {
            s.editor.update(cx, |st, cx| {
                st.set_cursor_position(
                    gpui_kit::component::input::Position::new(pos.line, pos.character),
                    window,
                    cx,
                )
            });
        }
    }

    /// Status bar, left: `ⓧ 2  ⚠ 1` (a check when clean) — opens Problems.
    pub(super) fn render_problems_button(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.theme();
        let (muted, fg, red, yellow) = (t.muted_foreground, t.foreground, t.red, t.yellow);
        let (errors, warnings) = self.problem_counts();
        let on = self.bottom_panel == Some(BottomPanel::Problems);
        let count = |icon: IconName, n: usize, c: Hsla| {
            div()
                .flex()
                .items_center()
                .gap_1()
                .child(
                    Icon::new(icon)
                        .size(px(12.))
                        .text_color(if n > 0 { c } else { muted }),
                )
                .child(n.to_string())
        };
        div()
            .id("status-problems")
            .cursor_pointer()
            .flex()
            .items_center()
            .gap_2()
            .px_1p5()
            .h(px(20.))
            .rounded(px(4.))
            .text_xs()
            .text_color(if on { fg } else { muted })
            .when(on, |d| d.bg(muted.opacity(0.12)))
            .hover(|d| d.bg(muted.opacity(0.12)).text_color(fg))
            .child(count(IconName::CircleX, errors, red))
            .child(count(IconName::TriangleAlert, warnings, yellow))
            .tooltip(|window, cx| {
                gpui_kit::component::tooltip::Tooltip::new("Problems").build(window, cx)
            })
            .on_click(
                cx.listener(|this, _, _, cx| this.toggle_bottom_panel(BottomPanel::Problems, cx)),
            )
            .into_any_element()
    }

    /// Status bar, right: the History toggle.
    pub(super) fn render_history_button(&self, cx: &mut Context<Self>) -> AnyElement {
        self.status_icon_button(
            "status-history",
            // The clock's circle fills its box; one px less to match optically.
            BottomPanel::History.icon().size(px(13.)),
            "Query History",
            "cmd-y",
            BottomPanel::History,
            cx,
        )
    }

    fn status_icon_button(
        &self,
        id: &'static str,
        icon: Icon,
        tip: &'static str,
        keys: &'static str,
        panel: BottomPanel,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = cx.theme();
        let (muted, fg, accent) = (t.muted_foreground, t.foreground, t.accent);
        let on = self.bottom_panel == Some(panel);
        div()
            .id(id)
            .cursor_pointer()
            .w(px(22.))
            .h(px(20.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(4.))
            .text_color(if on { accent } else { muted })
            .hover(|d| {
                d.bg(muted.opacity(0.12))
                    .text_color(if on { accent } else { fg })
            })
            .child(icon)
            .tooltip(move |window, cx| {
                gpui_kit::component::tooltip::Tooltip::new(tip)
                    .key_binding(crate::kbd::tip(keys))
                    .build(window, cx)
            })
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_bottom_panel(panel, cx)))
            .into_any_element()
    }

    /// Status bar, right: the Console toggle.
    pub(super) fn render_console_button(&self, cx: &mut Context<Self>) -> AnyElement {
        self.status_icon_button(
            "status-console",
            BottomPanel::Console.icon().size(px(14.)),
            "Console",
            "cmd-shift-c",
            BottomPanel::Console,
            cx,
        )
    }
}
