//! Explicit single-document Elasticsearch editing, never a transactional grid save.
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Root, TitleBar,
    button::Button,
    input::{Input, InputState, Textarea, TextareaState},
};
use gpui_kit::*;
use serde_json::Value;

pub struct DocumentEditor {
    db: crate::db::Db,
    index: Entity<InputState>,
    id: Entity<InputState>,
    source: Entity<TextareaState>,
    guard: Option<(u64, u64)>,
    loaded: Option<(String, String)>,
    busy: bool,
    deleting: bool,
    notice: String,
}
impl DocumentEditor {
    pub fn open(db: crate::db::Db, index: String, cx: &mut App) {
        let result = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(760.), px(620.)),
                    cx,
                ))),
                kind: crate::theme::secondary_window_kind(),
                ..TitleBar::window_options()
            },
            |window, cx| {
                let view = cx.new(|cx| Self {
                    db,
                    index: cx.new(|cx| InputState::new(window, cx).default_value(index)),
                    id: cx.new(|cx| {
                        InputState::new(window, cx).placeholder("Document ID (required)")
                    }),
                    source: cx.new(|cx| TextareaState::new(window, cx).default_value("{}")),
                    guard: None,
                    loaded: None,
                    busy: false,
                    deleting: false,
                    notice: "Load an existing document or create one with a new ID.".into(),
                });
                cx.new(|cx| Root::new(view, window, cx))
            },
        );
        if let Err(e) = result {
            log::warn!("Document editor: {e}");
        }
    }
    fn run(&mut self, mode: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let index = self.index.read(cx).value().to_string();
        let id = self.id.read(cx).value().to_string();
        if index.trim().is_empty() || id.trim().is_empty() {
            self.notice = "Index and document ID are required.".into();
            cx.notify();
            return;
        }
        if mode == "delete" && !self.deleting {
            self.deleting = true;
            self.notice = "Click Confirm Delete to delete this document.".into();
            cx.notify();
            return;
        }
        let source = if mode == "save" || mode == "create" {
            match serde_json::from_str::<Value>(&self.source.read(cx).value()) {
                Ok(v) if v.is_object() => Some(v),
                _ => {
                    self.notice = "Document must be a valid JSON object.".into();
                    cx.notify();
                    return;
                }
            }
        } else {
            None
        };
        let guard = if mode == "create" { None } else { self.guard };
        if matches!(mode, "save" | "delete")
            && (guard.is_none() || self.loaded.as_ref() != Some(&(index.clone(), id.clone())))
        {
            self.notice =
                "Load this document before editing or deleting; conflict protection is required."
                    .into();
            cx.notify();
            return;
        }
        self.busy = true;
        self.deleting = false;
        self.notice = "Working…".into();
        cx.notify();
        let db = self.db.clone();
        let index2 = index.clone();
        let id2 = id.clone();
        let task = crate::db::runtime().spawn(async move {
            if mode == "load" {
                db.driver().document_get(index2, id2).await
            } else {
                db.driver().document_write(index2, id2, source, guard).await
            }
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await.unwrap_or_else(|e| Err(e.to_string()));
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match result {
                    Ok(v) => {
                        this.guard = v["_seq_no"].as_u64().zip(v["_primary_term"].as_u64());
                        this.loaded = Some((index, id));
                        if mode == "load" {
                            this.source.update(cx, |s, cx| {
                                s.set_value(
                                    serde_json::to_string_pretty(&v["_source"]).unwrap_or_default(),
                                    window,
                                    cx,
                                )
                            });
                        }
                        if mode == "delete" {
                            this.guard = None;
                            this.loaded = None;
                        }
                        this.notice = if mode == "load" && this.guard.is_none() {
                            "Loaded without conflict protection; editing disabled.".into()
                        } else {
                            format!("Document {mode} completed.")
                        };
                    }
                    Err(e) => {
                        this.notice = format!(
                            "{e}\nYour document draft is preserved. Reload to resolve a conflict."
                        )
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
}
impl Render for DocumentEditor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(t.background)
            .text_color(t.foreground)
            .font_family(crate::settings::ui_font())
            .child(TitleBar::new())
            .child(
                div()
                    .p_4()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .flex_1()
                    .child("Elasticsearch Document Editor")
                    .child(Input::new(&self.index))
                    .child(Input::new(&self.id))
                    .child(
                        div().flex_1().min_h_0().child(
                            Textarea::new(&self.source)
                                .h_full()
                                .font_family(crate::settings::table_font()),
                        ),
                    )
                    .child(self.notice.clone())
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(
                                Button::new("doc-load")
                                    .label("Load")
                                    .disabled(self.busy)
                                    .on_click(cx.listener(|s, _, w, cx| s.run("load", w, cx))),
                            )
                            .child(
                                Button::new("doc-create")
                                    .label("Create")
                                    .disabled(self.busy)
                                    .on_click(cx.listener(|s, _, w, cx| s.run("create", w, cx))),
                            )
                            .child(
                                Button::new("doc-save")
                                    .label("Save")
                                    .disabled(self.busy || self.guard.is_none())
                                    .on_click(cx.listener(|s, _, w, cx| s.run("save", w, cx))),
                            )
                            .child(
                                Button::new("doc-delete")
                                    .label(if self.deleting {
                                        "Confirm Delete"
                                    } else {
                                        "Delete"
                                    })
                                    .disabled(self.busy || self.guard.is_none())
                                    .on_click(cx.listener(|s, _, w, cx| s.run("delete", w, cx))),
                            ),
                    ),
            )
    }
}
