//! Bounded asynchronous discovery for the Tools settings page.
use gpui_kit::*;
use std::{path::PathBuf, time::Duration};

#[derive(Default, Clone)]
pub struct Tools {
    pub scanning: bool,
    pub rows: Vec<(String, String)>,
}
impl Global for Tools {}
async fn inspect(name: &str, path: Option<PathBuf>) -> (String, String) {
    let Some(path) = path else {
        return (
            name.into(),
            "Not found — install the tool or configure its TUSK environment variable.".into(),
        );
    };
    let mut command = tokio::process::Command::new(&path);
    command.arg("--version").kill_on_drop(true);
    let status = match tokio::time::timeout(Duration::from_secs(3), command.output()).await {
        Ok(Ok(out)) if out.status.success() => {
            let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
            format!(
                "{} · {}",
                path.display(),
                version.chars().take(120).collect::<String>()
            )
        }
        Ok(Ok(_)) => format!(
            "{} · executable found; version probe failed",
            path.display()
        ),
        Ok(Err(_)) => format!("{} · could not start", path.display()),
        Err(_) => format!("{} · version probe timed out", path.display()),
    };
    (name.into(), status)
}
pub fn refresh(cx: &mut App) {
    if cx.default_global::<Tools>().scanning {
        return;
    }
    cx.global_mut::<Tools>().scanning = true;
    cx.refresh_windows();
    let task = crate::db::runtime().spawn(async {
        let mut rows = Vec::new();
        for name in ["postgres-language-server", "sqls"] {
            rows.push(inspect(name, crate::lsp::find_binary(name)).await);
        }
        let dirs = crate::backup::tool_dirs();
        for name in ["pg_dump", "pg_restore", "psql"] {
            let file = format!("{name}{}", std::env::consts::EXE_SUFFIX);
            let path = dirs.iter().map(|d| d.join(&file)).find(|p| p.is_file());
            rows.push(inspect(name, path).await);
        }
        rows
    });
    cx.spawn(async move |cx: &mut AsyncApp| {
        let rows = task.await.unwrap_or_else(|_| {
            vec![(
                "Discovery".into(),
                "Could not inspect tools. Try Refresh.".into(),
            )]
        });
        cx.update(|cx| {
            cx.set_global(Tools {
                scanning: false,
                rows,
            });
            cx.refresh_windows();
        });
    })
    .detach();
}
