//! "Migrate from TablePlus" sheet: review the connections found (cards with
//! their color, host, group and tag, all picked), then import them one by
//! one — each card fills with a check, a progress bar runs across — and end
//! on a summary with a popping check mark. Child module of `app`.
//! Also reviews / imports a Docker Compose file's database services
//! (`crate::compose`), with the same cards.

use std::time::Duration;

use gpui_kit::component::button::Button;
use gpui_kit::component::checkbox::Checkbox;

use super::*;
use crate::migrate::{Plan, Source};

pub enum Phase {
    Review,
    /// Card `current` is being imported; `done[i]` = imported with password?
    Importing {
        current: usize,
        done: Vec<Option<bool>>,
    },
    Done {
        imported: usize,
        passwords: usize,
        failed: Vec<String>,
    },
}

pub struct MigrateSheet {
    pub plan: Plan,
    pub picked: Vec<bool>,
    pub phase: Phase,
}

impl MigrateSheet {
    pub fn importing(&self) -> bool {
        matches!(self.phase, Phase::Importing { .. })
    }
}

/// A 0→1 animation driver for `with_animation`.
fn anim(ms: u64) -> Animation {
    Animation::new(Duration::from_millis(ms)).with_easing(ease_out_quint())
}

impl TuskApp {
    fn start_migration(&mut self, cx: &mut Context<Self>) {
        let Some(sheet) = self.migrate.as_mut() else {
            return;
        };
        let jobs: Vec<(usize, SavedConnection, String)> = sheet
            .plan
            .connections
            .iter()
            .enumerate()
            .filter(|(i, _)| sheet.picked.get(*i).copied().unwrap_or(false))
            .map(|(i, (c, id))| (i, c.clone(), id.clone()))
            .collect();
        if jobs.is_empty() {
            return;
        }
        let n = sheet.plan.connections.len();
        let source = sheet.plan.source.clone();
        sheet.phase = Phase::Importing {
            current: jobs[0].0,
            done: vec![None; n],
        };
        cx.notify();
        cx.spawn(async move |weak, cx: &mut AsyncApp| {
            let (mut imported, mut passwords, mut failed) = (0, 0, Vec::new());
            for (i, conn, id) in jobs {
                let _ = weak.update(cx, |this: &mut TuskApp, cx| {
                    if let Some(Phase::Importing { current, .. }) =
                        this.migrate.as_mut().map(|m| &mut m.phase)
                    {
                        *current = i;
                    }
                    cx.notify();
                });
                let name = conn.name.clone();
                let compose = source != Source::TablePlus;
                let result = cx
                    .background_executor()
                    .spawn(async move {
                        if compose {
                            crate::compose::import_one(&conn, &id)
                        } else {
                            crate::migrate::import_one(&conn, &id)
                        }
                    })
                    .await;
                // Let each card's check land before the next one starts.
                cx.background_executor()
                    .timer(Duration::from_millis(420))
                    .await;
                let _ = weak.update(cx, |this: &mut TuskApp, cx| {
                    if let Some(Phase::Importing { done, .. }) =
                        this.migrate.as_mut().map(|m| &mut m.phase)
                    {
                        match &result {
                            Ok(pw) => done[i] = Some(*pw),
                            Err(_) => done[i] = Some(false),
                        }
                    }
                    cx.notify();
                });
                match result {
                    Ok(pw) => {
                        imported += 1;
                        passwords += usize::from(pw);
                    }
                    Err(e) => failed.push(format!("{name}: {e}")),
                }
            }
            cx.background_executor()
                .timer(Duration::from_millis(350))
                .await;
            let _ = weak.update(cx, |this: &mut TuskApp, cx| {
                this.groups = db::load_groups();
                this.reload_saved_connections(cx);
                if let Some(m) = this.migrate.as_mut() {
                    m.phase = Phase::Done {
                        imported,
                        passwords,
                        failed,
                    };
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn migrate_card(&self, i: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(sheet) = &self.migrate else {
            return div().into_any_element();
        };
        let (conn, _) = &sheet.plan.connections[i];
        let t = cx.theme();
        let (fg, muted, border, accent, green) = (
            t.foreground,
            t.muted_foreground,
            t.border,
            t.accent,
            t.green,
        );
        let tint: Hsla = conn.status_rgb().map_or(muted, |c| rgb(c).into());
        let picked = sheet.picked[i];
        let (state, is_current): (Option<Option<bool>>, bool) = match &sheet.phase {
            Phase::Importing { current, done } => {
                (Some(done[i]), *current == i && done[i].is_none())
            }
            Phase::Done { .. } => (Some(Some(true)), false),
            Phase::Review => (None, false),
        };
        let finished = matches!(state, Some(Some(_)));
        let compose = sheet.plan.source != Source::TablePlus;
        let mut detail = if compose {
            // Compose cards: which engine, where it is published.
            let mut d = format!("{} · {}:{}", conn.engine.label(), conn.host, conn.port);
            if !conn.database.is_empty() {
                d.push_str(&format!(" · {}", conn.database));
            }
            d
        } else {
            format!("{} · {}", conn.host, conn.database)
        };
        if conn.ssh.is_some() {
            detail.push_str(" · ssh");
        }
        let badge = |text: String, c: Hsla| {
            div()
                .px_1p5()
                .h(px(18.))
                .flex()
                .items_center()
                .rounded(px(4.))
                .bg(c.opacity(0.16))
                .text_xs()
                .text_color(c)
                .child(text)
        };
        // Right side: checkbox while reviewing, spinner-dot while importing,
        // an animated check when done.
        let trailing: AnyElement = match state {
            None => Checkbox::new(("mig-pick", i))
                .checked(picked)
                .on_click(cx.listener(move |this, v: &bool, _, cx| {
                    if let Some(m) = this.migrate.as_mut() {
                        m.picked[i] = *v;
                    }
                    cx.notify();
                }))
                .into_any_element(),
            Some(None) if is_current => div()
                .size(px(10.))
                .rounded_full()
                .bg(accent)
                .with_animation(
                    ("mig-pulse", i),
                    Animation::new(Duration::from_millis(900))
                        .repeat()
                        .with_easing(bounce(ease_in_out)),
                    |d, delta| d.opacity(0.35 + 0.65 * delta).size(px(8. + 4. * delta)),
                )
                .into_any_element(),
            Some(None) => div()
                .size(px(8.))
                .rounded_full()
                .bg(muted.opacity(0.35))
                .into_any_element(),
            Some(Some(_)) => div()
                .size(px(20.))
                .rounded_full()
                .bg(green)
                .flex()
                .items_center()
                .justify_center()
                .child(
                    Icon::new(IconName::Check)
                        .size(px(12.))
                        .text_color(gpui::black()),
                )
                .with_animation(("mig-check", i), anim(420), |d, delta| {
                    d.opacity(delta).size(px(8. + 12. * delta))
                })
                .into_any_element(),
        };
        let dim = state.is_none() && !picked;
        div()
            .id(("mig-card", i))
            .flex()
            .items_center()
            .gap_3()
            .px_3()
            .h(px(52.))
            .rounded(px(8.))
            .border_1()
            .border_color(if is_current {
                accent.opacity(0.6)
            } else {
                border
            })
            .bg(if finished {
                green.opacity(0.06)
            } else {
                fg.opacity(0.03)
            })
            .when(dim, |d| d.opacity(0.45))
            .child(if compose {
                crate::icons::engine_badge(conn.engine, 30.)
            } else {
                // Status color disc with the database glyph.
                div()
                    .size(px(30.))
                    .flex_none()
                    .rounded(px(8.))
                    .bg(tint.opacity(0.22))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(Icon::new(IconName::Database).size(px(15.)).text_color(tint))
                    .into_any_element()
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1p5()
                            .child(div().text_sm().text_color(fg).child(conn.name.clone()))
                            .children(
                                conn.folder
                                    .clone()
                                    .filter(|_| !compose)
                                    .map(|f| badge(f, muted)),
                            )
                            .children(conn.tag.map(|tag| {
                                badge(tag.label().to_string(), rgb(tag.color()).into())
                            })),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_xs()
                            .font_family(crate::settings::table_font())
                            .text_color(muted.opacity(0.7))
                            .child(detail),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(24.))
                    .flex()
                    .justify_center()
                    .child(trailing),
            )
            // Cards slide up in sequence when the sheet opens.
            .with_animation(("mig-card-in", i), anim(380 + 70 * i as u64), |d, delta| {
                d.opacity(delta).mt(px(10. * (1. - delta)))
            })
            .into_any_element()
    }

    pub(super) fn render_migrate(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(sheet) = &self.migrate else {
            return div().into_any_element();
        };
        let t = cx.theme().clone();
        let (bg, fg, muted, border, accent, green) = (
            t.popover,
            t.foreground,
            t.muted_foreground,
            t.border,
            t.accent,
            t.green,
        );
        let n = sheet.plan.connections.len();
        let cards: Vec<AnyElement> = (0..n).map(|i| self.migrate_card(i, cx)).collect();
        let picked = sheet.picked.iter().filter(|p| **p).count();

        let header = div()
            .flex()
            .flex_col()
            .items_center()
            .gap_1()
            .pt_6()
            .pb_4()
            .child(
                // TablePlus → Tusk: two tiles with an arrow flowing between.
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(match &sheet.plan.source {
                        // A neutral tile: we don't ship other products' logos.
                        Source::TablePlus => div()
                            .size(px(40.))
                            .flex_none()
                            .rounded(px(9.))
                            .bg(rgb(0x3A3A44))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(Icon::new(IconName::ArrowDownToLine).size(px(22.)).text_color(gpui::white()))
                            .into_any_element(),
                        Source::Compose(_) => div()
                            .size(px(40.))
                            .flex_none()
                            .rounded(px(9.))
                            .bg(rgb(0x1D63ED))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                Icon::default()
                                    .data(include_bytes!("../assets/icons/ui/docker.svg"))
                                    .size(px(26.))
                                    .text_color(gpui::white()),
                            )
                            .into_any_element(),
                    })
                    .child(
                        Icon::new(IconName::ArrowRight)
                            .size(px(16.))
                            .text_color(accent)
                            .with_animation(
                                "mig-arrow",
                                Animation::new(Duration::from_millis(1400)).repeat().with_easing(bounce(ease_in_out)),
                                |i, delta| i.opacity(0.4 + 0.6 * delta),
                            ),
                    )
                    .child(DbIcon::Postgres.icon_px(40.)),
            )
            .child(
                div()
                    .pt_3()
                    .text_base()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(fg)
                    .child(match &sheet.phase {
                        Phase::Review => "Bring your connections over".to_string(),
                        Phase::Importing { .. } => "Moving in…".to_string(),
                        Phase::Done { .. } => "All set".to_string(),
                    }),
            )
            .child(div().text_sm().text_color(muted).child(match &sheet.phase {
                Phase::Review => match &sheet.plan.source {
                    Source::TablePlus => "Groups, tags, colors, SSH and saved passwords come along.".to_string(),
                    Source::Compose(project) => {
                        format!("Database services of “{project}” go into the {project} folder, passwords included.")
                    }
                },
                Phase::Importing { done, .. } => format!(
                    "{} of {} imported",
                    done.iter().filter(|d| d.is_some()).count(),
                    sheet.picked.iter().filter(|p| **p).count()
                ),
                Phase::Done { imported, passwords, .. } => {
                    format!("{imported} connection(s) and {passwords} password(s) imported")
                }
            }));

        // Progress bar (importing / done).
        let progress: Option<AnyElement> = match &sheet.phase {
            Phase::Review => None,
            Phase::Importing { done, .. } => {
                let total = picked.max(1) as f32;
                let frac = done.iter().filter(|d| d.is_some()).count() as f32 / total;
                Some(
                    div()
                        .mx_6()
                        .h(px(4.))
                        .rounded_full()
                        .bg(fg.opacity(0.08))
                        .child(
                            div()
                                .h_full()
                                .rounded_full()
                                .bg(accent)
                                .w(relative(frac.max(0.04))),
                        )
                        .into_any_element(),
                )
            }
            Phase::Done { .. } => Some(
                div()
                    .mx_6()
                    .h(px(4.))
                    .rounded_full()
                    .bg(green)
                    .into_any_element(),
            ),
        };

        let body: AnyElement = match &sheet.phase {
            Phase::Done {
                imported, failed, ..
            } if failed.is_empty() || *imported > 0 => div()
                .flex()
                .flex_col()
                .items_center()
                .gap_3()
                .py_6()
                .child(
                    div()
                        .size(px(64.))
                        .rounded_full()
                        .bg(green.opacity(0.18))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            div()
                                .size(px(40.))
                                .rounded_full()
                                .bg(green)
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(
                                    Icon::new(IconName::Check)
                                        .size(px(22.))
                                        .text_color(gpui::black()),
                                ),
                        )
                        .with_animation(
                            "mig-done",
                            Animation::new(Duration::from_millis(650))
                                .with_easing(ease_out_quint()),
                            |d, delta| {
                                // Overshoot, then settle.
                                let s = 1. + 0.18 * (delta * std::f32::consts::PI).sin();
                                d.size(px(64. * s * delta.max(0.2))).opacity(delta)
                            },
                        ),
                )
                .children(
                    failed
                        .iter()
                        .map(|f| div().text_xs().text_color(t.red).child(f.clone())),
                )
                .into_any_element(),
            _ => div()
                .id("mig-list")
                .max_h(px(320.))
                .overflow_y_scroll()
                .px_6()
                .flex()
                .flex_col()
                .gap_2()
                .children(cards)
                .when(
                    matches!(sheet.phase, Phase::Review)
                        && sheet.plan.source != Source::TablePlus
                        && !sheet.plan.skipped.is_empty(),
                    |d| {
                        d.child(
                            div()
                                .pt_1()
                                .text_xs()
                                .text_color(muted)
                                .child(format!("Skipped: {}", sheet.plan.skipped.join(", "))),
                        )
                    },
                )
                .into_any_element(),
        };

        let footer = div()
            .flex()
            .items_center()
            .justify_end()
            .gap_2()
            .px_6()
            .py_4()
            .map(|d| match &sheet.phase {
                Phase::Review => d
                    .child(
                        Button::new("mig-cancel")
                            .label("Cancel")
                            .small()
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.migrate = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("mig-go")
                            .label(if picked == 1 {
                                "Import 1 Connection".to_string()
                            } else {
                                format!("Import {picked} Connections")
                            })
                            .small()
                            .primary()
                            .disabled(picked == 0)
                            .on_click(cx.listener(|this, _, _, cx| this.start_migration(cx))),
                    ),
                Phase::Importing { .. } => d.child(div().h(px(24.))),
                Phase::Done { .. } => d.child(
                    Button::new("mig-close")
                        .label("Open Connections")
                        .small()
                        .outline()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.migrate = None;
                            this.conn_manager = true;
                            cx.notify();
                        })),
                ),
            });

        let backdrop = gpui_kit::black().opacity(if t.is_dark() { 0.5 } else { 0.25 });
        div()
            .id("migrate-backdrop")
            .absolute()
            .inset_0()
            .bg(backdrop)
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .id("migrate-card")
                    .w(px(520.))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .rounded(px(14.))
                    .border_1()
                    .border_color(border)
                    .bg(bg)
                    .shadow_lg()
                    .overflow_hidden()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(header)
                    .children(progress)
                    .child(body)
                    .child(footer)
                    .with_animation("migrate-in", anim(320), |d, delta| {
                        d.opacity(delta).mt(px(18. * (1. - delta)))
                    }),
            )
            .into_any_element()
    }
}
