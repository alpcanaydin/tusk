//! Explicit provider configuration; secret values are saved only to the OS credential store.
use crate::{acp_registry::CustomAgent, http_ai::Provider};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Root, TitleBar,
    button::Button,
    input::{Input, InputState},
};
use gpui_kit::*;

pub struct ProviderEditor {
    id: String,
    acp: bool,
    name: Entity<InputState>,
    endpoint: Entity<InputState>,
    model: Entity<InputState>,
    command: Entity<InputState>,
    args: Entity<InputState>,
    env: Entity<InputState>,
    key: Entity<InputState>,
    tools: bool,
    status: String,
    busy: bool,
}
impl ProviderEditor {
    pub fn open(id: String, acp: bool, cx: &mut App) {
        let prefs = crate::settings::get();
        let p = prefs.ai_http.get(&id).cloned().unwrap_or_default();
        let a = prefs.agent_servers.get(&id).cloned().unwrap_or_default();
        let result=cx.open_window(WindowOptions{window_bounds:Some(WindowBounds::Windowed(Bounds::centered(None,size(px(720.),px(640.)),cx))),kind:crate::theme::secondary_window_kind(),..TitleBar::window_options()},|window,cx|{
            let view=cx.new(|cx|{
                let mut input=|value:String,secret:bool,cx:&mut App|cx.new(|cx|InputState::new(window,cx).default_value(value).masked(secret));
                Self{id,acp,name:input(if acp{a.name}else{p.name},false,cx),endpoint:input(p.base_url,false,cx),model:input(p.model,false,cx),command:input(a.command,false,cx),args:input(serde_json::to_string(&a.args).unwrap_or_default(),false,cx),env:input(String::new(),true,cx),key:input(String::new(),true,cx),tools:p.tools,status:"API keys and new environment values are stored in your OS credential store. Leave secret fields empty to preserve existing values.".into(),busy:false}
            });cx.new(|cx|Root::new(view,window,cx))
        });
        if let Err(e) = result {
            log::warn!("AI settings window: {e}");
        }
    }
    fn provider(&self, cx: &App) -> Provider {
        Provider {
            name: self.name.read(cx).value().to_string(),
            base_url: self.endpoint.read(cx).value().to_string(),
            model: self.model.read(cx).value().to_string(),
            tools: self.tools,
            key_required: crate::settings::get()
                .ai_http
                .get(&self.id)
                .is_some_and(|p| p.key_required)
                || !self.key.read(cx).value().is_empty(),
        }
    }
    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let id = self.id.clone();
        let p = self.provider(cx);
        let command = self.command.read(cx).value().to_string();
        let args = match serde_json::from_str::<Vec<String>>(&self.args.read(cx).value()) {
            Ok(v) => v,
            Err(e) if self.acp => {
                self.status = format!("Arguments must be a JSON array of strings: {e}");
                cx.notify();
                return;
            }
            Err(_) => vec![],
        };
        let env_text = self.env.read(cx).value().to_string();
        let env = if env_text.is_empty() {
            Default::default()
        } else {
            match serde_json::from_str::<std::collections::BTreeMap<String, String>>(&env_text) {
                Ok(v) => v,
                Err(e) => {
                    self.status =
                        format!("Environment must be a JSON object of string values: {e}");
                    cx.notify();
                    return;
                }
            }
        };
        if self.acp && command.trim().is_empty() {
            self.status = "An ACP executable is required.".into();
            cx.notify();
            return;
        }
        if !self.acp && crate::http_ai::validate_endpoint(&p).is_err() {
            self.status = "Enter a valid endpoint URL.".into();
            cx.notify();
            return;
        }
        let key = self.key.read(cx).value().to_string();
        let acp = self.acp;
        let old = crate::settings::get()
            .agent_servers
            .get(&id)
            .cloned()
            .unwrap_or_default();
        self.busy = true;
        cx.notify();
        let task = crate::db::runtime().spawn_blocking(move || -> Result<_, String> {
            if !key.is_empty() {
                crate::db::save_password(&crate::http_ai::secret_id(&format!("http:{id}")), &key)?;
            }
            let mut agent = CustomAgent {
                name: p.name.clone(),
                command,
                args,
                env: old.env,
                secret_env: old.secret_env,
            };
            for (k, v) in env {
                crate::db::save_password(&format!("tusk-ai-env:{id}:{k}"), &v)?;
                agent.env.remove(&k);
                if !agent.secret_env.contains(&k) {
                    agent.secret_env.push(k);
                }
            }
            Ok((id, p, agent, acp))
        });
        cx.spawn_in(window,async move|this,cx|{let result=task.await.unwrap_or_else(|e|Err(e.to_string()));let _=this.update_in(cx,|this,window,cx|{this.busy=false;match result{Ok((id,p,a,acp))=>{crate::settings::update(cx,|prefs|{if acp{prefs.agent_servers.insert(id,a);}else{prefs.ai_http.insert(id,p);}});this.status="Saved. Choose this provider in the assistant to start a new conversation.".into();this.key.update(cx,|s,cx|s.set_value("",window,cx));this.env.update(cx,|s,cx|s.set_value("",window,cx));},Err(e)=>this.status=format!("Could not save secrets: {e}")};cx.notify();});}).detach();
    }
    fn test(&mut self, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let p = self.provider(cx);
        let key = self.key.read(cx).value().to_string();
        let id = self.id.clone();
        let acp = self.acp;
        let command = self.command.read(cx).value().to_string();
        let args = self.args.read(cx).value().to_string();
        let environment = self.env.read(cx).value().to_string();
        let saved = crate::settings::get()
            .agent_servers
            .get(&id)
            .cloned()
            .unwrap_or_default();
        self.busy = true;
        self.status = "Testing connection…".into();
        cx.notify();
        let task = crate::db::runtime().spawn(async move {
            if acp {
                let program = if std::path::Path::new(&command).is_file() {
                    std::path::PathBuf::from(&command)
                } else {
                    crate::acp_registry::which(&command)
                        .ok_or("Executable missing. Install the agent or enter its full path.")?
                };
                let args = serde_json::from_str::<Vec<String>>(&args)
                    .map_err(|e| format!("Invalid arguments: {e}"))?;
                let mut env = saved.env;
                for name in saved.secret_env {
                    let secret = crate::db::load_password(&format!("tusk-ai-env:{id}:{name}"))?;
                    env.insert(name, secret);
                }
                if !environment.is_empty() {
                    env.extend(
                        serde_json::from_str::<std::collections::BTreeMap<String, String>>(
                            &environment,
                        )
                        .map_err(|e| format!("Invalid environment: {e}"))?,
                    );
                }
                let cwd = crate::db::app_dir().join("agent-probe");
                std::fs::create_dir_all(&cwd).map_err(|e| e.to_string())?;
                let (connection, _incoming) = crate::acp::AcpConnection::start(
                    &crate::acp::AgentCommand {
                        program,
                        args,
                        env: env.into_iter().collect(),
                    },
                    &cwd,
                )
                .map_err(|e| e.to_string())?;
                tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    connection.request("initialize", crate::acp::initialize_params()),
                )
                .await
                .map_err(|_| "ACP startup timed out.")?
                .map_err(|e| e.message)?;
                let result = tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    connection.request(
                        "session/new",
                        serde_json::json!({"cwd":cwd.display().to_string(),"mcpServers":[]}),
                    ),
                )
                .await
                .map_err(|_| "ACP session setup timed out.")?;
                return match result {
                    Ok(_) => Ok("Ready. ACP initialization and session setup succeeded.".into()),
                    Err(e) if e.code == crate::acp::AUTH_REQUIRED => Ok(
                        "Login required. Select this agent in the assistant to complete its login."
                            .into(),
                    ),
                    Err(e) => Err(e.message),
                };
            }
            let key = if key.is_empty() {
                crate::http_ai::load_key(&p, &format!("http:{id}"))?
            } else {
                key
            };
            let models = crate::http_ai::models(&p, &key).await?;
            if models.is_empty() {
                Ok(
                    "Server connected, but no models are available. Load a model in the server."
                        .into(),
                )
            } else {
                Ok(format!(
                    "Available models: {}",
                    models.into_iter().take(20).collect::<Vec<_>>().join(", ")
                ))
            }
        });
        cx.spawn(async move |this, cx| {
            let result = task.await.unwrap_or_else(|e| Err(e.to_string()));
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                this.status = result.unwrap_or_else(|e| e);
                cx.notify();
            });
        })
        .detach();
    }
}
impl Render for ProviderEditor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = cx.theme();
        let mut body = div()
            .p_4()
            .flex()
            .flex_col()
            .gap_3()
            .child("AI Provider")
            .child("Name")
            .child(Input::new(&self.name));
        if self.acp {
            body = body
                .child("ACP executable")
                .child(Input::new(&self.command))
                .child("Arguments (JSON array)")
                .child(Input::new(&self.args))
                .child("Secret environment values (JSON object; blank preserves saved values)")
                .child(Input::new(&self.env));
        } else {
            body = body
                .child("Base URL (the destination for chat and selected context)")
                .child(Input::new(&self.endpoint))
                .child("Model ID")
                .child(Input::new(&self.model))
                .child("API key (optional; blank preserves saved key)")
                .child(Input::new(&self.key))
                .child(
                    Button::new("ai-tools")
                        .label(if self.tools {
                            "Schema tools: enabled"
                        } else {
                            "Schema tools: disabled (text assistance)"
                        })
                        .on_click(cx.listener(|s, _, _, cx| {
                            s.tools = !s.tools;
                            cx.notify();
                        })),
                );
        }
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(t.background)
            .text_color(t.foreground)
            .font_family(crate::settings::ui_font())
            .child(TitleBar::new())
            .child(
                body.id("ai-provider-form")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(self.status.clone())
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(
                                Button::new("ai-test")
                                    .label("Test Connection / Discover Models")
                                    .disabled(self.busy)
                                    .on_click(cx.listener(|s, _, _, cx| s.test(cx))),
                            )
                            .child(
                                Button::new("ai-save")
                                    .label("Save")
                                    .disabled(self.busy)
                                    .on_click(cx.listener(|s, _, w, cx| s.save(w, cx))),
                            ),
                    ),
            )
    }
}
