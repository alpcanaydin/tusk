//! SSH tunnel for "Over SSH" connections, via `russh`.
//!
//! `open` connects + authenticates to the jump host, then listens on a
//! random `127.0.0.1` port; every TCP connection to it (sqlx pool
//! connections, the language server) is forwarded through an SSH
//! `direct-tcpip` channel to the database host:port *as seen from the jump
//! host*. Dropping the [`Tunnel`] closes the listener and the SSH session.
//!
//! Host keys follow OpenSSH semantics against `~/.ssh/known_hosts`: a known
//! matching key passes, an unknown host is trusted on first use and
//! recorded, a changed key is refused (possible MITM).

use std::path::PathBuf;
use std::sync::Arc;

use russh::client;
use russh::keys::known_hosts::{check_known_hosts_path, learn_known_hosts_path};
use russh::keys::{PrivateKeyWithHashAlg, PublicKeyOrCertificate, load_secret_key};
use tokio::net::TcpListener;

use crate::db::SshConfig;

struct Client {
    host: String,
    port: u16,
}

impl client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        // Host certificates aren't pinned in known_hosts: accept like OpenSSH
        // does once the CA is trusted (not configurable here yet).
        let key = match key {
            PublicKeyOrCertificate::PublicKey { key, .. } => key,
            PublicKeyOrCertificate::Certificate(_) => return Ok(true),
        };
        let file = known_hosts_file();
        match check_known_hosts_path(&self.host, self.port, key, &file) {
            Ok(true) => Ok(true),
            // Unknown host: trust on first use, like `ssh` with accept-new.
            Ok(false) => {
                let _ = learn_known_hosts_path(&self.host, self.port, key, &file);
                Ok(true)
            }
            // Recorded key differs: refuse.
            Err(_) => Ok(false),
        }
    }
}

/// A live tunnel; keep it for as long as the pool uses it.
pub struct Tunnel {
    pub local_port: u16,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// `~/.ssh/known_hosts`, or `$TUSK_KNOWN_HOSTS` (tests use a temp file).
fn known_hosts_file() -> PathBuf {
    std::env::var_os("TUSK_KNOWN_HOSTS")
        .map(PathBuf::from)
        .unwrap_or_else(|| expand_home("~/.ssh/known_hosts"))
}

fn expand_home(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => dirs::home_dir().unwrap_or_default().join(rest),
        None => PathBuf::from(path),
    }
}

/// Connect to the jump host and start forwarding `127.0.0.1:<local_port>`
/// → `target_host:target_port`. `secret` is the SSH password, or the key
/// passphrase when `cfg.key_path` is set. Runs on the tokio runtime.
pub async fn open(
    cfg: SshConfig,
    secret: Option<String>,
    target_host: String,
    target_port: u16,
) -> Result<Tunnel, String> {
    let config = Arc::new(client::Config {
        inactivity_timeout: None,
        keepalive_interval: Some(std::time::Duration::from_secs(30)),
        nodelay: true,
        ..Default::default()
    });
    let handler = Client {
        host: cfg.host.clone(),
        port: cfg.port,
    };
    let mut session = client::connect(config, (cfg.host.as_str(), cfg.port), handler)
        .await
        .map_err(|e| format!("SSH connect to {}:{} failed: {e}", cfg.host, cfg.port))?;

    let auth = match &cfg.key_path {
        Some(path) => {
            let key = load_secret_key(
                expand_home(path),
                secret.as_deref().filter(|s| !s.is_empty()),
            )
            .map_err(|e| format!("SSH key {path}: {e}"))?;
            let hash = session
                .best_supported_rsa_hash()
                .await
                .map_err(|e| e.to_string())?
                .flatten();
            session
                .authenticate_publickey(&cfg.user, PrivateKeyWithHashAlg::new(Arc::new(key), hash))
                .await
        }
        None => {
            session
                .authenticate_password(&cfg.user, secret.unwrap_or_default())
                .await
        }
    }
    .map_err(|e| format!("SSH authentication error: {e}"))?;
    if !auth.success() {
        return Err(format!(
            "SSH authentication failed for {}@{}",
            cfg.user, cfg.host
        ));
    }

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| format!("local tunnel port: {e}"))?;
    let local_port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let session = Arc::new(session);
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, peer)) = listener.accept().await else {
                break;
            };
            let session = session.clone();
            let target_host = target_host.clone();
            tokio::spawn(async move {
                let channel = session
                    .channel_open_direct_tcpip(
                        target_host,
                        u32::from(target_port),
                        peer.ip().to_string(),
                        u32::from(peer.port()),
                    )
                    .await;
                if let Ok(channel) = channel {
                    let mut stream = channel.into_stream();
                    let _ = tokio::io::copy_bidirectional(&mut socket, &mut stream).await;
                }
            });
        }
    });
    Ok(Tunnel { local_port, task })
}

#[cfg(test)]
mod tests {
    use super::expand_home;

    #[test]
    fn tilde_expands_to_home() {
        let p = expand_home("~/.ssh/id_ed25519");
        assert!(p.ends_with(".ssh/id_ed25519") && !p.to_string_lossy().starts_with('~'));
        assert_eq!(expand_home("/etc/key").to_string_lossy(), "/etc/key");
    }
}
