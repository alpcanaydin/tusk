//! Triggers and DDL views of a table tab (bottom bar "Triggers" / "DDL").

use super::*;
use crate::objects::{ObjKind, Script};

impl TuskApp {
    /// DDL view: the table's full `CREATE` statement, read-only (selectable,
    /// copyable), re-read each time the view opens.
    pub(super) fn load_ddl(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(pool), Some(tab)) = (self.pool.clone(), self.grid_tab(ix)) else {
            return;
        };
        let (schema, name) = (tab.table.schema.clone(), tab.table.name.clone());
        let kind = match tab.table.kind {
            TableKind::View => ObjKind::View,
            TableKind::MaterializedView => ObjKind::MatView,
            _ => ObjKind::Table,
        };
        let editor = match tab.ddl.clone() {
            Some(e) => e,
            None => {
                let e = cx.new(|cx| {
                    let mut st = gpui_kit::component::input::EditorState::new(window, cx)
                        .language("sql")
                        .line_number(true)
                        .soft_wrap(false);
                    st.set_readonly(true, cx);
                    st
                });
                if let Some(t) = self.grid_tab_mut(ix) {
                    t.ddl = Some(e.clone());
                }
                e
            }
        };
        cx.spawn_in(window, async move |this, cx| {
            let text = crate::objects::script(&pool, kind, &schema, &name, Script::Create).await;
            let _ = this.update_in(cx, |this, window, cx| {
                match text {
                    Ok(sql) => editor.update(cx, |st, cx| {
                        st.set_readonly(false, cx);
                        st.set_value(sql, window, cx);
                        st.set_readonly(true, cx);
                    }),
                    Err(e) => this.toast(false, format!("DDL: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Triggers view: the table's triggers, re-read each time it opens.
    pub(super) fn load_triggers(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(pool), Some(tab)) = (self.pool.clone(), self.grid_tab(ix)) else {
            return;
        };
        let (schema, table) = (tab.table.schema.clone(), tab.table.name.clone());
        let st = match tab.triggers.clone() {
            Some(st) => st,
            None => {
                let st = crate::sql::new_result_state(window, cx);
                st.update(cx, |s, _| {
                    s.delegate_mut().empty_text = "No triggers".into()
                });
                if let Some(t) = self.grid_tab_mut(ix) {
                    t.triggers = Some(st.clone());
                }
                st
            }
        };
        cx.spawn(async move |weak, cx: &mut AsyncApp| {
            let rows = db::fetch_triggers(&pool, &schema, &table).await;
            let _ = weak.update(cx, |this: &mut TuskApp, cx| {
                match rows {
                    Ok(rows) => {
                        let n = rows.len();
                        let cols = if rows.is_empty() {
                            [
                                "trigger_name",
                                "timing",
                                "event",
                                "level",
                                "function",
                                "enabled",
                                "definition",
                            ]
                            .iter()
                            .map(|c| crate::sql::QCol {
                                name: c.to_string(),
                                right: false,
                            })
                            .collect()
                        } else {
                            crate::sql::infer_columns(&rows)
                        };
                        st.update(cx, |s, cx| {
                            s.delegate_mut()
                                .set_result(cols, crate::sql::rows_to_vec(rows), None);
                            s.refresh(cx);
                            cx.notify();
                        });
                        if let Some(t) = this.grid_tab_mut(ix) {
                            t.trigger_count = Some(n);
                        }
                    }
                    Err(e) => this.toast(false, format!("Triggers: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// "+ Trigger": a function + trigger template in a query tab.
    pub(super) fn new_trigger_editor(
        &mut self,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tab) = self.grid_tab(ix) else { return };
        let q = db::quote_ident;
        let (schema, table) = (tab.table.schema.clone(), tab.table.name.clone());
        let func = format!("{}.{}", q(&schema), q(&format!("{table}_trigger_fn")));
        let sql = format!(
            "CREATE OR REPLACE FUNCTION {func}()\nRETURNS trigger\nLANGUAGE plpgsql\nAS $$\nBEGIN\n    RETURN NEW;\nEND;\n$$;\n\nCREATE TRIGGER {}\nBEFORE INSERT OR UPDATE ON {}.{}\nFOR EACH ROW EXECUTE FUNCTION {func}();",
            q(&format!("{table}_trigger")),
            q(&schema),
            q(&table),
        );
        self.open_sql_tab_with(Some(sql), window, cx);
    }
}
