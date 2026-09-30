//! Agents for the AI panel: the public ACP registry (npm packages, uv
//! packages, prebuilt archives) plus agents the user adds in settings.
//!
//! The registry list is cached under the app's data dir and refreshed once a
//! day; installs land in `agents/<kind>/<id>/<version>` next to it. Downloads
//! go through the system `curl`, archives are checked against their sha256
//! and unpacked with `unzip` / `tar`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use anyhow::{Context as _, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::acp::AgentCommand;

pub const REGISTRY_URL: &str =
    "https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json";
const REFRESH: Duration = Duration::from_secs(24 * 60 * 60);

/// One agent the panel can start.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentSpec {
    pub id: String,
    pub name: String,
    pub description: String,
    pub version: String,
    pub source: Source,
    /// Local copy of the registry icon (an SVG), once downloaded.
    pub icon: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    Npx {
        package: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
    },
    Uvx {
        package: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
    },
    Binary {
        archive: String,
        cmd: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
        sha256: Option<String>,
    },
    /// Added by the user in settings: run as is.
    Custom(AgentCommand),
}

/// An agent from settings (`agent_servers`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct CustomAgent {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: std::collections::BTreeMap<String, String>,
}

pub fn data_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("tusk")
        .join("agents")
}

/// PATH for agents and installers: the inherited one plus the places node,
/// npm, uv and friends usually live (a Finder-launched app gets a bare PATH).
pub fn search_path() -> std::ffi::OsString {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    if let Some(home) = dirs::home_dir() {
        for d in [
            ".local/bin",
            ".cargo/bin",
            "Library/pnpm",
            ".bun/bin",
            ".volta/bin",
            ".npm-global/bin",
        ] {
            dirs.push(home.join(d));
        }
        // The newest nvm node, then a node another editor already downloaded.
        for base in [
            home.join(".nvm/versions/node"),
            home.join("Library/Application Support/Zed/node"),
        ] {
            if let Ok(rd) = std::fs::read_dir(&base) {
                let mut vs: Vec<PathBuf> = rd.flatten().map(|e| e.path().join("bin")).collect();
                vs.sort();
                dirs.extend(vs.into_iter().rev());
            }
        }
    }
    // Windows: the node installer and npm's global shims.
    for (var, sub) in [("ProgramFiles", "nodejs"), ("APPDATA", "npm")] {
        if let Some(base) = std::env::var_os(var) {
            dirs.push(PathBuf::from(base).join(sub));
        }
    }
    for d in [
        "/opt/homebrew/bin",
        "/usr/local/bin",
        "/usr/bin",
        "/bin",
        "/usr/sbin",
        "/sbin",
    ] {
        dirs.push(d.into());
    }
    let mut seen = std::collections::HashSet::new();
    dirs.retain(|d| seen.insert(d.clone()));
    std::env::join_paths(dirs).unwrap_or_default()
}

/// `name` on [`search_path`] (on Windows `name.exe`, else the npm-style
/// `name.cmd` shim).
pub fn which(name: &str) -> Option<PathBuf> {
    let exts: &[&str] = if cfg!(windows) {
        &[".exe", ".cmd"]
    } else {
        &[""]
    };
    std::env::split_paths(&search_path())
        .flat_map(|d| exts.iter().map(move |e| d.join(format!("{name}{e}"))))
        .find(|p| p.is_file())
}

fn env_pairs(v: &Value) -> Vec<(String, String)> {
    v.as_object()
        .map(|o| {
            o.iter()
                .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

fn str_list(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn platform() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "darwin-aarch64",
        ("macos", _) => "darwin-x86_64",
        ("linux", "aarch64") => "linux-aarch64",
        ("linux", _) => "linux-x86_64",
        (_, "aarch64") => "windows-aarch64",
        _ => "windows-x86_64",
    }
}

/// The agents of a registry document this machine can run.
pub fn parse_registry(doc: &Value) -> Vec<AgentSpec> {
    let Some(agents) = doc.get("agents").and_then(Value::as_array) else {
        return Vec::new();
    };
    agents
        .iter()
        .filter_map(|a| {
            let dist = a.get("distribution")?;
            // Prefer a prebuilt binary, then npm, then uv.
            let source = if let Some(b) = dist.get("binary").and_then(|b| b.get(platform())) {
                Source::Binary {
                    archive: b.get("archive")?.as_str()?.to_string(),
                    cmd: b.get("cmd")?.as_str()?.to_string(),
                    args: str_list(&b["args"]),
                    env: env_pairs(&b["env"]),
                    sha256: b.get("sha256").and_then(Value::as_str).map(str::to_string),
                }
            } else if let Some(n) = dist.get("npx") {
                Source::Npx {
                    package: n.get("package")?.as_str()?.to_string(),
                    args: str_list(&n["args"]),
                    env: env_pairs(&n["env"]),
                }
            } else {
                let u = dist.get("uvx")?;
                Source::Uvx {
                    package: u.get("package")?.as_str()?.to_string(),
                    args: str_list(&u["args"]),
                    env: env_pairs(&u["env"]),
                }
            };
            let id = a.get("id")?.as_str()?.to_string();
            let icon = data_dir().join("icons").join(format!("{id}.svg"));
            Some(AgentSpec {
                name: a
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or(&id)
                    .to_string(),
                description: a
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                version: a
                    .get("version")
                    .and_then(Value::as_str)
                    .unwrap_or("latest")
                    .to_string(),
                icon: icon.is_file().then_some(icon),
                id,
                source,
            })
        })
        .collect()
}

/// Agents to offer before the registry was ever reached.
fn fallback() -> Vec<AgentSpec> {
    [
        (
            "claude-acp",
            "Claude Agent",
            "@agentclientprotocol/claude-agent-acp",
            vec![],
        ),
        (
            "codex-acp",
            "Codex",
            "@agentclientprotocol/codex-acp",
            vec![],
        ),
        (
            "gemini",
            "Gemini CLI",
            "@google/gemini-cli",
            vec!["--acp".to_string()],
        ),
    ]
    .into_iter()
    .map(|(id, name, package, args)| AgentSpec {
        id: id.into(),
        name: name.into(),
        description: String::new(),
        version: "latest".into(),
        source: Source::Npx {
            package: package.into(),
            args,
            env: Vec::new(),
        },
        icon: None,
    })
    .collect()
}

pub fn custom_specs(custom: &std::collections::BTreeMap<String, CustomAgent>) -> Vec<AgentSpec> {
    custom
        .iter()
        .filter(|(_, c)| !c.command.trim().is_empty())
        .map(|(id, c)| {
            let program = PathBuf::from(&c.command);
            let program = if program.is_absolute() {
                program
            } else {
                which(&c.command).unwrap_or(program)
            };
            AgentSpec {
                id: format!("custom:{id}"),
                name: if c.name.is_empty() {
                    id.clone()
                } else {
                    c.name.clone()
                },
                description: c.command.clone(),
                version: String::new(),
                source: Source::Custom(AgentCommand {
                    program,
                    args: c.args.clone(),
                    env: c.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                }),
                icon: None,
            }
        })
        .collect()
}

fn registry_cache() -> PathBuf {
    data_dir().join("registry.json")
}

/// The cached list (or the built-in one), without touching the network.
pub fn cached() -> Vec<AgentSpec> {
    std::fs::read(registry_cache())
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .map(|v| parse_registry(&v))
        .filter(|l| !l.is_empty())
        .unwrap_or_else(fallback)
}

fn stale() -> bool {
    std::fs::metadata(registry_cache())
        .and_then(|m| m.modified())
        .map(|t| SystemTime::now().duration_since(t).unwrap_or_default() > REFRESH)
        .unwrap_or(true)
}

/// Blocking: refresh the cached registry when it is a day old (and fetch
/// missing icons). Returns the list either way.
pub fn refresh(force: bool) -> Vec<AgentSpec> {
    if force || stale() {
        let _ = std::fs::create_dir_all(data_dir());
        let tmp = registry_cache().with_extension("tmp");
        if curl(REGISTRY_URL, &tmp).is_ok()
            && std::fs::read(&tmp)
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
                .is_some_and(|v| !parse_registry(&v).is_empty())
        {
            let _ = std::fs::rename(&tmp, registry_cache());
        }
        let _ = std::fs::remove_file(&tmp);
    }
    let doc = std::fs::read(registry_cache())
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
    if let Some(agents) = doc
        .as_ref()
        .and_then(|d| d.get("agents"))
        .and_then(Value::as_array)
    {
        let icons = data_dir().join("icons");
        let _ = std::fs::create_dir_all(&icons);
        for a in agents {
            let (Some(id), Some(url)) = (
                a.get("id").and_then(Value::as_str),
                a.get("icon").and_then(Value::as_str),
            ) else {
                continue;
            };
            let path = icons.join(format!("{id}.svg"));
            if !path.exists() {
                let _ = curl(url, &path);
            }
        }
    }
    cached()
}

fn curl(url: &str, out: &Path) -> Result<()> {
    // URLs come from the downloaded registry: https only, and never parsed
    // as a curl option.
    if !url.starts_with("https://") {
        bail!("refusing to download {url}");
    }
    let st = Command::new("curl")
        .args(["-fsSL", "--proto", "=https", "--max-time", "300", "-o"])
        .arg(out)
        .arg("--")
        .arg(url)
        .status()
        .context("curl")?;
    if !st.success() {
        let _ = std::fs::remove_file(out);
        bail!("download failed: {url}");
    }
    Ok(())
}

/// `@scope/name@1.2.3` → (`@scope/name`, `1.2.3`).
pub fn split_package(spec: &str) -> (&str, Option<&str>) {
    match spec.rfind('@') {
        Some(i) if i > 0 => (&spec[..i], Some(&spec[i + 1..])),
        _ => (spec, None),
    }
}

fn install_dir(kind: &str, spec: &AgentSpec) -> PathBuf {
    let v = if spec.version.is_empty() {
        "latest"
    } else {
        &spec.version
    };
    data_dir().join(kind).join(&spec.id).join(v)
}

/// Is the agent ready to start without an install step?
pub fn installed(spec: &AgentSpec) -> bool {
    match &spec.source {
        Source::Npx { .. } => install_dir("npx", spec).join(".ok").exists(),
        Source::Binary { .. } => install_dir("bin", spec).join(".ok").exists(),
        // uv fetches the package on first run: "installed" once it started.
        Source::Uvx { .. } => install_dir("uvx", spec).join(".ok").exists(),
        Source::Custom(_) => true,
    }
}

/// Blocking: install the agent when needed and return how to start it.
pub fn resolve(spec: &AgentSpec) -> Result<AgentCommand> {
    match &spec.source {
        Source::Custom(cmd) => Ok(cmd.clone()),
        Source::Uvx { package, args, env } => {
            let uvx =
                which("uvx").ok_or_else(|| anyhow!("{} needs uv (uvx) installed", spec.name))?;
            let mut a = vec![package.clone()];
            a.extend(args.iter().cloned());
            let dir = install_dir("uvx", spec);
            if std::fs::create_dir_all(&dir).is_ok() {
                let _ = std::fs::write(dir.join(".ok"), b"");
            }
            Ok(AgentCommand {
                program: uvx,
                args: a,
                env: env.clone(),
            })
        }
        Source::Npx { package, args, env } => {
            let dir = install_dir("npx", spec);
            if !dir.join(".ok").exists() {
                if package.starts_with('-') {
                    bail!("refusing package name {package}");
                }
                let npm = which("npm").ok_or_else(|| {
                    anyhow!(
                        "{} needs Node.js — install it from nodejs.org, then try again",
                        spec.name
                    )
                })?;
                std::fs::create_dir_all(&dir)?;
                let out = Command::new(npm)
                    .env("PATH", search_path())
                    .args([
                        "install",
                        "--no-audit",
                        "--no-fund",
                        "--loglevel=error",
                        "--prefix",
                    ])
                    .arg(&dir)
                    .arg("--")
                    .arg(package)
                    .output()
                    .context("npm install")?;
                if !out.status.success() {
                    bail!(
                        "npm install {package} failed:\n{}",
                        String::from_utf8_lossy(&out.stderr).trim()
                    );
                }
                std::fs::write(dir.join(".ok"), b"")?;
            }
            let (name, _) = split_package(package);
            let pkg_dir = dir.join("node_modules").join(name);
            let bin = npm_bin(&pkg_dir, name)?;
            Ok(AgentCommand {
                program: pkg_dir.join(bin),
                args: args.clone(),
                env: env.clone(),
            })
        }
        Source::Binary {
            archive,
            cmd,
            args,
            env,
            sha256,
        } => {
            let dir = install_dir("bin", spec);
            if !dir.join(".ok").exists() {
                std::fs::create_dir_all(&dir)?;
                let file_name = archive.rsplit('/').next().unwrap_or("archive");
                let file = dir.join(file_name);
                curl(archive, &file)?;
                if let Some(want) = sha256 {
                    use sha2::Digest as _;
                    let got = hex::encode(sha2::Sha256::digest(std::fs::read(&file)?));
                    if !got.eq_ignore_ascii_case(want) {
                        let _ = std::fs::remove_file(&file);
                        bail!("{} download failed its checksum", spec.name);
                    }
                }
                // Windows has no unzip, but its bundled bsdtar reads zips.
                let status = if file_name.ends_with(".zip") && !cfg!(windows) {
                    Command::new("unzip")
                        .args(["-q", "-o"])
                        .arg(&file)
                        .arg("-d")
                        .arg(&dir)
                        .status()?
                } else if file_name.contains(".tar")
                    || file_name.ends_with(".tgz")
                    || file_name.ends_with(".zip")
                {
                    Command::new("tar")
                        .arg("xf")
                        .arg(&file)
                        .arg("-C")
                        .arg(&dir)
                        .status()?
                } else {
                    // A bare executable.
                    std::fs::rename(&file, dir.join(cmd.trim_start_matches("./")))?;
                    std::process::ExitStatus::default()
                };
                if !status.success() {
                    bail!("unpacking {file_name} failed");
                }
                let _ = std::fs::remove_file(&file);
                std::fs::write(dir.join(".ok"), b"")?;
            }
            let program = dir.join(cmd.trim_start_matches("./"));
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                let _ = std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755));
            }
            Ok(AgentCommand {
                program,
                args: args.clone(),
                env: env.clone(),
            })
        }
    }
}

/// The executable an npm package declares (`bin` string or map).
fn npm_bin(pkg_dir: &Path, name: &str) -> Result<String> {
    let manifest: Value = serde_json::from_slice(
        &std::fs::read(pkg_dir.join("package.json")).context("package.json")?,
    )?;
    let short = name.rsplit('/').next().unwrap_or(name);
    match &manifest["bin"] {
        Value::String(s) => Ok(s.clone()),
        Value::Object(m) => m
            .get(short)
            .or_else(|| m.values().next())
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| anyhow!("{name} has no executable")),
        _ => bail!("{name} has no executable"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_registry_sources() {
        let doc = json!({"version":"1","agents":[
            {"id":"a","name":"A","version":"1.0","distribution":{"npx":{"package":"@x/a@1.0","args":["--acp"]}}},
            {"id":"b","name":"B","version":"2","distribution":{"binary":{
                "darwin-aarch64":{"archive":"https://e/b.zip","cmd":"./b","args":["acp"]},
                "darwin-x86_64":{"archive":"https://e/b.zip","cmd":"./b","args":["acp"]},
                "linux-x86_64":{"archive":"https://e/b.tgz","cmd":"./b"},
                "linux-aarch64":{"archive":"https://e/b.tgz","cmd":"./b"},
                "windows-x86_64":{"archive":"https://e/b.zip","cmd":"./b"},
                "windows-aarch64":{"archive":"https://e/b.zip","cmd":"./b"}}}},
            {"id":"c","distribution":{"uvx":{"package":"c"}}},
            {"id":"d","distribution":{}}
        ]});
        let l = parse_registry(&doc);
        assert_eq!(l.len(), 3);
        assert!(
            matches!(&l[0].source, Source::Npx { package, args, .. } if package == "@x/a@1.0" && args == &["--acp"])
        );
        assert!(matches!(&l[1].source, Source::Binary { cmd, .. } if cmd == "./b"));
        assert!(matches!(&l[2].source, Source::Uvx { .. }));
        assert_eq!(l[2].name, "c");
    }

    #[test]
    fn package_names() {
        assert_eq!(split_package("@a/b@1.2"), ("@a/b", Some("1.2")));
        assert_eq!(split_package("@a/b"), ("@a/b", None));
        assert_eq!(split_package("pkg@3"), ("pkg", Some("3")));
    }
}
