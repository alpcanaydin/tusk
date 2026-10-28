//! The connection switcher in the title bar (child module of `app`): a
//! search box over the saved connections, then New / Backup / Restore /
//! Disconnect. The kit's menu has no search, and a long list is hard to scan.

use gpui_kit::component::input::InputEvent;

use super::*;

const WIDTH: f32 = 340.;
/// Rows shown before the list scrolls.
const SHOWN: usize = 12;

struct Row {
    /// Index into the saved connections.
    ix: usize,
    name: String,
    endpoint: String,
    status: Option<u32>,
}

pub struct ConnPicker {
    rows: Vec<Row>,
    active: String,
    query: Entity<InputState>,
    /// Indexes into `rows` that match the query.
    matches: Vec<usize>,
    pick: usize,
    scroll: UniformListScrollHandle,
    _sub: Subscription,
}

impl ConnPicker {
    pub fn new(
        saved: &[SavedConnection],
        active: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let query = cx.new(|cx| InputState::new(window, cx).placeholder("Search connections…"));
        let _sub = cx.subscribe_in(
            &query,
            window,
            |this, _, ev: &InputEvent, window, cx| match ev {
                InputEvent::Change => this.refilter(cx),
                InputEvent::PressEnter { .. } => this.confirm(None, window, cx),
                _ => {}
            },
        );
        let rows: Vec<Row> = saved
            .iter()
            .enumerate()
            .map(|(ix, c)| Row {
                ix,
                name: c.name.clone(),
                endpoint: c.endpoint(),
                status: c.status_rgb(),
            })
            .collect();
        let pick = rows.iter().position(|r| r.name == active).unwrap_or(0);
        let scroll = UniformListScrollHandle::new();
        scroll.scroll_to_item(pick, ScrollStrategy::Center);
        Self {
            matches: (0..rows.len()).collect(),
            rows,
            active,
            query,
            pick,
            scroll,
            _sub,
        }
    }

    pub fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.query.read(cx).focus_handle(cx)
    }

    fn refilter(&mut self, cx: &mut Context<Self>) {
        let q = self.query.read(cx).value().to_lowercase();
        let words: Vec<&str> = q.split_whitespace().collect();
        self.matches = (0..self.rows.len())
            .filter(|&i| {
                let r = &self.rows[i];
                let hay = format!("{} {}", r.name, r.endpoint).to_lowercase();
                words.iter().all(|w| hay.contains(w))
            })
            .collect();
        self.pick = 0;
        self.scroll.scroll_to_item(0, ScrollStrategy::Top);
        cx.notify();
    }

    fn step(&mut self, delta: isize, cx: &mut Context<Self>) {
        let n = self.matches.len();
        if n == 0 {
            return;
        }
        self.pick = (self.pick as isize + delta).rem_euclid(n as isize) as usize;
        self.scroll
            .scroll_to_item(self.pick, ScrollStrategy::Nearest);
        cx.notify();
    }

    fn close(cx: &mut App) {
        let view = cx.global::<TuskHandle>().0.clone();
        view.update(cx, |app, cx| {
            app.conn_picker = None;
            cx.notify();
        });
    }

    /// Switch to the highlighted connection, or to the one at `row`.
    fn confirm(&mut self, row: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(&m) = self.matches.get(row.unwrap_or(self.pick)) else {
            return;
        };
        let ix = self.rows[m].ix;
        let view = cx.global::<TuskHandle>().0.clone();
        view.update(cx, |app, cx| {
            app.conn_picker = None;
            app.switch_connection_guarded(ix, window, cx);
            cx.notify();
        });
    }

    fn run(&mut self, action: Box<dyn Action>, window: &mut Window, cx: &mut Context<Self>) {
        Self::close(cx);
        window.dispatch_action(action, cx);
    }
}

impl Render for ConnPicker {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme().clone();
        let (fg, muted) = (t.foreground, t.muted_foreground);
        let row_h = crate::settings::row_h();
        let n = self.matches.len();
        let this = cx.entity();
        let list = uniform_list("conn-picker-list", n, move |range, _, cx| {
            let p = this.read(cx);
            range
                .map(|r| {
                    let row = &p.rows[p.matches[r]];
                    let selected = row.name == p.active;
                    let status: Hsla = row.status.map_or(muted.opacity(0.4), |c| rgb(c).into());
                    let this = this.clone();
                    div()
                        .id(("conn-picker-row", r))
                        .w_full()
                        .h(px(row_h))
                        .px_2()
                        .mx_1()
                        .rounded(crate::theme::RADIUS_SM)
                        .flex()
                        .items_center()
                        .gap_2()
                        .text_size(px(crate::settings::ui_text()))
                        .when(r == p.pick, |d| d.bg(t.tokens.table_active))
                        .hover(|d| d.bg(muted.opacity(0.12)))
                        .child(div().w(px(12.)).flex_none().when(selected, |d| {
                            d.child(Icon::new(IconName::Check).size(px(12.)).text_color(fg))
                        }))
                        .child(div().size(px(7.)).flex_none().rounded_full().bg(status))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_color(fg)
                                .child(row.name.clone()),
                        )
                        .child(
                            div()
                                .flex_none()
                                .max_w(px(130.))
                                .truncate()
                                .text_caption()
                                .text_color(muted)
                                .child(row.endpoint.clone()),
                        )
                        .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                            this.update(cx, |p, cx| p.confirm(Some(r), window, cx));
                        })
                })
                .collect::<Vec<_>>()
        })
        .track_scroll(&self.scroll)
        .h(px(n.clamp(1, SHOWN) as f32 * row_h));

        let action_row = |id: &'static str, label: &'static str, make: fn() -> Box<dyn Action>| {
            div()
                .id(id)
                .w_full()
                .h(px(row_h))
                .px_2()
                .mx_1()
                .rounded(crate::theme::RADIUS_SM)
                .flex()
                .items_center()
                .text_size(px(crate::settings::ui_text()))
                .text_color(fg)
                .hover(|d| d.bg(muted.opacity(0.12)))
                .child(label)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, window, cx| this.run(make(), window, cx)),
                )
        };

        div()
            .w(px(WIDTH))
            .flex()
            .flex_col()
            .gap_1()
            .key_context("ConnPicker")
            .on_action(cx.listener(|this, _: &ConnPickUp, _, cx| this.step(-1, cx)))
            .on_action(cx.listener(|this, _: &ConnPickDown, _, cx| this.step(1, cx)))
            .child(Input::new(&self.query).small())
            .child(if n == 0 {
                div()
                    .p_2()
                    .text_caption()
                    .text_color(muted)
                    .child("No match")
                    .into_any_element()
            } else {
                list.into_any_element()
            })
            .child(div().h(px(1.)).mx_1().bg(t.border))
            .child(action_row("conn-picker-new", "New Connection…", || {
                Box::new(NewConnection)
            }))
            .child(action_row(
                "conn-picker-backup",
                "Backup Database…",
                || Box::new(BackupDatabase),
            ))
            .child(action_row(
                "conn-picker-restore",
                "Restore Database…",
                || Box::new(RestoreDatabase),
            ))
            .child(div().h(px(1.)).mx_1().bg(t.border))
            .child(action_row("conn-picker-disconnect", "Disconnect", || {
                Box::new(Disconnect)
            }))
    }
}

impl TuskApp {
    /// Open the connection switcher and focus its search box.
    pub(super) fn open_conn_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let saved = self.form.saved.clone();
        let active = self.active_name.clone();
        let picker = cx.new(|cx| ConnPicker::new(&saved, active, window, cx));
        let focus = picker.read(cx).focus_handle(cx);
        window.defer(cx, move |window, cx| focus.focus(window, cx));
        self.conn_picker = Some(picker);
        cx.notify();
    }
}
