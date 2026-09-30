//! Tools ▸ User Management: the server's roles (system `pg_*` roles hidden),
//! each as a form of its attributes; New Role / Drop / Save.

use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::InputEvent;

use super::*;

const SQL_ROLES: &str = "SELECT r.rolname AS name, r.rolsuper, r.rolinherit, r.rolcreaterole,
        r.rolcreatedb, r.rolcanlogin, r.rolreplication, r.rolbypassrls, r.rolconnlimit,
        to_char(r.rolvaliduntil, 'YYYY-MM-DD HH24:MI:SS') AS valid_until,
        COALESCE((SELECT string_agg(g.rolname, ', ' ORDER BY g.rolname)
            FROM pg_auth_members m JOIN pg_roles g ON g.oid = m.roleid
           WHERE m.member = r.oid), '') AS member_of
   FROM pg_roles r
  WHERE r.rolname !~ '^pg_'
  ORDER BY r.rolname";

/// One role's attributes as loaded / edited.
#[derive(Clone, Debug, PartialEq, Default)]
pub(super) struct RoleAttrs {
    pub name: String,
    pub superuser: bool,
    pub inherit: bool,
    pub create_role: bool,
    pub create_db: bool,
    pub login: bool,
    pub replication: bool,
    pub bypass_rls: bool,
    pub conn_limit: i64,
    pub valid_until: Option<String>,
    pub member_of: String,
}

impl RoleAttrs {
    fn from_json(v: &serde_json::Value) -> Self {
        let b = |k: &str| v.get(k).and_then(|x| x.as_bool()).unwrap_or(false);
        let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
        Self {
            name: s("name").unwrap_or_default(),
            superuser: b("rolsuper"),
            inherit: b("rolinherit"),
            create_role: b("rolcreaterole"),
            create_db: b("rolcreatedb"),
            login: b("rolcanlogin"),
            replication: b("rolreplication"),
            bypass_rls: b("rolbypassrls"),
            conn_limit: v.get("rolconnlimit").and_then(|x| x.as_i64()).unwrap_or(-1),
            valid_until: s("valid_until"),
            member_of: s("member_of").unwrap_or_default(),
        }
    }

    /// The attribute clause of CREATE / ALTER ROLE.
    fn options(&self) -> String {
        let f = |on: bool, yes: &str, no: &str| if on { yes.to_string() } else { no.to_string() };
        let mut out = vec![
            f(self.superuser, "SUPERUSER", "NOSUPERUSER"),
            f(self.inherit, "INHERIT", "NOINHERIT"),
            f(self.create_role, "CREATEROLE", "NOCREATEROLE"),
            f(self.create_db, "CREATEDB", "NOCREATEDB"),
            f(self.login, "LOGIN", "NOLOGIN"),
            f(self.replication, "REPLICATION", "NOREPLICATION"),
            f(self.bypass_rls, "BYPASSRLS", "NOBYPASSRLS"),
            format!("CONNECTION LIMIT {}", self.conn_limit),
        ];
        out.push(match &self.valid_until {
            Some(v) if !v.trim().is_empty() => {
                format!("VALID UNTIL {}", db::quote_literal(v.trim()))
            }
            _ => "VALID UNTIL 'infinity'".into(),
        });
        out.join(" ")
    }
}

pub(super) struct UserMgmt {
    pub roles: Vec<RoleAttrs>,
    /// Index into `roles`, or `None` for a role being created.
    pub selected: Option<usize>,
    pub creating: bool,
    pub draft: RoleAttrs,
    pub name_input: Entity<InputState>,
    pub password: Entity<InputState>,
    pub limit: Entity<InputState>,
    pub valid: Entity<InputState>,
    _subs: Vec<Subscription>,
}

impl TuskApp {
    pub(super) fn open_user_mgmt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pool.is_none() {
            return;
        }
        let input = |placeholder: &'static str, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        let name_input = input("role_name", window, cx);
        let password = input("unchanged", window, cx);
        let limit = input("-1 (no limit)", window, cx);
        let valid = input("never expires", window, cx);
        let subs = vec![
            cx.subscribe(&limit, |this, i, ev: &InputEvent, cx| {
                if matches!(ev, InputEvent::Change)
                    && let Some(u) = this.users.as_mut()
                {
                    u.draft.conn_limit = i.read(cx).value().trim().parse().unwrap_or(-1);
                }
            }),
            cx.subscribe(&valid, |this, i, ev: &InputEvent, cx| {
                if matches!(ev, InputEvent::Change)
                    && let Some(u) = this.users.as_mut()
                {
                    let v = i.read(cx).value().trim().to_string();
                    u.draft.valid_until = (!v.is_empty()).then_some(v);
                }
            }),
        ];
        self.users = Some(UserMgmt {
            roles: Vec::new(),
            selected: None,
            creating: false,
            draft: RoleAttrs::default(),
            name_input,
            password,
            limit,
            valid,
            _subs: subs,
        });
        self.load_roles(None, window, cx);
    }

    fn load_roles(&mut self, select: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pool) = self.pool.clone() else {
            return;
        };
        cx.spawn_in(window, async move |this, cx| {
            let rows = db::run_query_rows(&pool, SQL_ROLES, 5_000).await;
            let _ = this.update_in(cx, |this, window, cx| {
                match rows {
                    Ok(rows) => {
                        let roles: Vec<RoleAttrs> = rows.iter().map(RoleAttrs::from_json).collect();
                        let ix = select
                            .and_then(|n| roles.iter().position(|r| r.name == n))
                            .or(if roles.is_empty() { None } else { Some(0) });
                        if let Some(u) = this.users.as_mut() {
                            u.roles = roles;
                        }
                        if let Some(ix) = ix {
                            this.select_role(ix, window, cx);
                        }
                    }
                    Err(e) => this.toast(false, format!("Roles: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn fill_role_form(
        &mut self,
        r: RoleAttrs,
        creating: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(u) = self.users.as_mut() else { return };
        u.creating = creating;
        let (name, pw, limit, valid) = (
            u.name_input.clone(),
            u.password.clone(),
            u.limit.clone(),
            u.valid.clone(),
        );
        name.update(cx, |st, cx| st.set_value(r.name.clone(), window, cx));
        pw.update(cx, |st, cx| st.set_value("", window, cx));
        limit.update(cx, |st, cx| {
            st.set_value(r.conn_limit.to_string(), window, cx)
        });
        valid.update(cx, |st, cx| {
            st.set_value(r.valid_until.clone().unwrap_or_default(), window, cx)
        });
        if let Some(u) = self.users.as_mut() {
            u.draft = r;
        }
        cx.notify();
    }

    fn select_role(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(r) = self.users.as_ref().and_then(|u| u.roles.get(ix).cloned()) else {
            return;
        };
        if let Some(u) = self.users.as_mut() {
            u.selected = Some(ix);
        }
        self.fill_role_form(r, false, window, cx);
    }

    fn new_role(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let r = RoleAttrs {
            name: "new_role".into(),
            login: true,
            inherit: true,
            conn_limit: -1,
            ..Default::default()
        };
        if let Some(u) = self.users.as_mut() {
            u.selected = None;
        }
        self.fill_role_form(r, true, window, cx);
    }

    /// CREATE ROLE / ALTER ROLE (+ RENAME, PASSWORD) for the form.
    fn save_role(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(pool), Some(u)) = (self.pool.clone(), self.users.as_ref()) else {
            return;
        };
        let name = u.name_input.read(cx).value().trim().to_string();
        if name.is_empty() {
            self.toast(false, "Name the role first.");
            cx.notify();
            return;
        }
        let password = u.password.read(cx).value().to_string();
        let q = db::quote_ident;
        let mut stmts = Vec::new();
        let opts = u.draft.options();
        let pw =
            (!password.is_empty()).then(|| format!(" PASSWORD {}", db::quote_literal(&password)));
        if u.creating {
            stmts.push(Stmt::plain(format!(
                "CREATE ROLE {} {opts}{}",
                q(&name),
                pw.unwrap_or_default()
            )));
        } else {
            let Some(orig) = u.selected.and_then(|i| u.roles.get(i)) else {
                return;
            };
            if orig.name != name {
                stmts.push(Stmt::plain(format!(
                    "ALTER ROLE {} RENAME TO {}",
                    q(&orig.name),
                    q(&name)
                )));
            }
            stmts.push(Stmt::plain(format!(
                "ALTER ROLE {} {opts}{}",
                q(&name),
                pw.unwrap_or_default()
            )));
        }
        cx.spawn_in(window, async move |this, cx| {
            let r = db::execute_batch(&pool, stmts).await;
            let _ = this.update_in(cx, |this, window, cx| {
                match r {
                    Ok(_) => {
                        this.toast(true, format!("Saved role {name}"));
                        this.load_roles(Some(name), window, cx);
                    }
                    Err(e) => this.toast(false, format!("Role not saved: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn drop_role(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(pool), Some(u)) = (self.pool.clone(), self.users.as_ref()) else {
            return;
        };
        let Some(name) = u
            .selected
            .and_then(|i| u.roles.get(i))
            .map(|r| r.name.clone())
        else {
            return;
        };
        let answer = window.prompt(
            PromptLevel::Critical,
            &format!("Drop role “{name}”?"),
            Some("Objects it owns must be reassigned or dropped first."),
            &["Drop", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            let r = db::run_exec(&pool, &format!("DROP ROLE {}", db::quote_ident(&name))).await;
            let _ = this.update_in(cx, |this, window, cx| {
                match r {
                    Ok(_) => {
                        this.toast(true, format!("Dropped role {name}"));
                        this.load_roles(None, window, cx);
                    }
                    Err(e) => this.toast(false, format!("Drop role {name}: {e}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn render_user_mgmt(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(u) = &self.users else {
            return div().into_any_element();
        };
        let t = cx.theme();
        let (border, bg, fg, muted) = (t.border, t.popover, t.foreground, t.muted_foreground);
        let backdrop = gpui_kit::black().opacity(if t.is_dark() { 0.45 } else { 0.2 });
        let d = u.draft.clone();
        let flag =
            |id: &'static str, label: &'static str, on: bool, set: fn(&mut RoleAttrs, bool)| {
                Checkbox::new(id)
                    .label(label)
                    .checked(on)
                    .on_click(cx.listener(move |this, v: &bool, _, cx| {
                        if let Some(u) = this.users.as_mut() {
                            set(&mut u.draft, *v);
                        }
                        cx.notify();
                    }))
            };
        let field = |label: &'static str, input: &Entity<InputState>| {
            div()
                .flex()
                .items_center()
                .gap_3()
                .child(div().w(px(110.)).text_sm().text_color(muted).child(label))
                .child(div().flex_1().child(Input::new(input).small()))
        };
        let list = div()
            .id("roles-list")
            .w(px(220.))
            .flex_none()
            .h_full()
            .overflow_y_scroll()
            .border_r_1()
            .border_color(border)
            .py_2()
            .children(u.roles.iter().enumerate().map(|(i, r)| {
                let on = u.selected == Some(i) && !u.creating;
                div()
                    .id(("role-row", i))
                    .cursor_pointer()
                    .mx_2()
                    .px_2()
                    .h(px(crate::settings::row_h()))
                    .flex()
                    .items_center()
                    .gap_2()
                    .rounded(crate::theme::RADIUS_SM)
                    .text_size(px(crate::settings::ui_text()))
                    .text_color(fg)
                    .when(on, |d| d.bg(muted.opacity(0.18)))
                    .hover(|d| d.bg(muted.opacity(0.1)))
                    .child(Icon::new(IconName::User).size(px(13.)).text_color(muted))
                    .child(div().flex_1().truncate().child(r.name.clone()))
                    .when(r.superuser, |d| {
                        d.child(div().text_caption().text_color(muted).child("super"))
                    })
                    .on_click(
                        cx.listener(move |this, _, window, cx| this.select_role(i, window, cx)),
                    )
            }));
        let form = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .child(field("Role name", &u.name_input))
            .child(field("Password", &u.password))
            .child(field("Conn. limit", &u.limit))
            .child(field("Valid until", &u.valid))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_x_4()
                    .gap_y_2()
                    .pt_1()
                    .child(flag("r-login", "Can log in", d.login, |r, v| r.login = v))
                    .child(flag("r-super", "Superuser", d.superuser, |r, v| {
                        r.superuser = v
                    }))
                    .child(flag(
                        "r-createdb",
                        "Create databases",
                        d.create_db,
                        |r, v| r.create_db = v,
                    ))
                    .child(flag(
                        "r-createrole",
                        "Create roles",
                        d.create_role,
                        |r, v| r.create_role = v,
                    ))
                    .child(flag(
                        "r-inherit",
                        "Inherit privileges",
                        d.inherit,
                        |r, v| r.inherit = v,
                    ))
                    .child(flag("r-repl", "Replication", d.replication, |r, v| {
                        r.replication = v
                    }))
                    .child(flag("r-rls", "Bypass RLS", d.bypass_rls, |r, v| {
                        r.bypass_rls = v
                    })),
            )
            .when(!d.member_of.is_empty(), |f| {
                f.child(
                    div()
                        .flex()
                        .gap_3()
                        .child(
                            div()
                                .w(px(110.))
                                .text_sm()
                                .text_color(muted)
                                .child("Member of"),
                        )
                        .child(div().text_sm().text_color(fg).child(d.member_of.clone())),
                )
            });
        div()
            .id("users-backdrop")
            .absolute()
            .inset_0()
            .bg(backdrop)
            .flex()
            .items_start()
            .justify_center()
            .pt(px(56.))
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.users = None;
                    cx.notify();
                }),
            )
            .child(
                div()
                    .id("users-card")
                    .w(px(760.))
                    .h(px(460.))
                    .flex()
                    .flex_col()
                    .rounded(crate::theme::RADIUS_LG)
                    .border_1()
                    .border_color(border)
                    .bg(bg)
                    .shadow_lg()
                    .overflow_hidden()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        div()
                            .h(px(44.))
                            .flex_none()
                            .flex()
                            .items_center()
                            .gap_2()
                            .px_4()
                            .border_b_1()
                            .border_color(border)
                            .child(
                                div()
                                    .flex_1()
                                    .text_sm()
                                    .text_color(fg)
                                    .child("User Management"),
                            )
                            .child(
                                Button::new("role-new")
                                    .label("New Role")
                                    .small()
                                    .outline()
                                    .on_click(
                                        cx.listener(|this, _, window, cx| {
                                            this.new_role(window, cx)
                                        }),
                                    ),
                            )
                            .child(
                                Button::new("role-drop")
                                    .label("Drop")
                                    .small()
                                    .danger()
                                    .disabled(u.creating || u.selected.is_none())
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.drop_role(window, cx)
                                    })),
                            )
                            .child(
                                Button::new("role-save")
                                    .label(if u.creating { "Create" } else { "Save" })
                                    .small()
                                    .primary()
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.save_role(window, cx)
                                    })),
                            )
                            .child(
                                Button::new("role-close")
                                    .icon(IconName::Close)
                                    .small()
                                    .ghost()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.users = None;
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child(div().flex_1().min_h_0().flex().child(list).child(form)),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::RoleAttrs;

    #[test]
    fn role_options_clause() {
        let r = RoleAttrs {
            name: "app".into(),
            login: true,
            inherit: true,
            conn_limit: 10,
            valid_until: Some("2030-01-01".into()),
            ..Default::default()
        };
        assert_eq!(
            r.options(),
            "NOSUPERUSER INHERIT NOCREATEROLE NOCREATEDB LOGIN NOREPLICATION NOBYPASSRLS CONNECTION LIMIT 10 VALID UNTIL '2030-01-01'"
        );
    }
}
