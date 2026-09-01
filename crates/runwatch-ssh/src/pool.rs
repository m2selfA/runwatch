use anyhow::{Context, Result, bail};
use russh::client::{self, Handle};
use russh::keys::agent::client::{AgentClient, AgentStream};
use russh::keys::{PrivateKeyWithHashAlg, PublicKeyBase64, load_public_key, load_secret_key};
use russh::{ChannelMsg, Disconnect};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

use crate::config::{SshHost, parse_ssh_config, resolve_effective_host};

#[derive(Debug, Clone)]
struct KnownHostsHandler {
    lookup_host: String,
    port: u16,
    paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct JumpSpec {
    host: String,
    user: Option<String>,
    port: Option<u16>,
}

fn parse_jump_spec(value: &str) -> Result<JumpSpec> {
    let value = value.trim();
    if value.is_empty() {
        bail!("empty ProxyJump hop")
    }
    let (user, authority) = match value.rsplit_once('@') {
        Some((user, authority)) if !user.is_empty() && !authority.is_empty() => {
            (Some(user.to_string()), authority)
        }
        Some(_) => bail!("invalid ProxyJump hop `{value}`"),
        None => (None, value),
    };

    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let end = rest
            .find(']')
            .with_context(|| format!("unterminated IPv6 ProxyJump hop `{value}`"))?;
        let host = &rest[..end];
        let suffix = &rest[end + 1..];
        let port = if suffix.is_empty() {
            None
        } else if let Some(port) = suffix.strip_prefix(':') {
            Some(
                port.parse::<u16>()
                    .with_context(|| format!("invalid ProxyJump port in `{value}`"))?,
            )
        } else {
            bail!("invalid ProxyJump suffix in `{value}`")
        };
        (host.to_string(), port)
    } else if authority.matches(':').count() == 1 {
        let (host, port) = authority.rsplit_once(':').expect("single colon");
        if host.is_empty() {
            bail!("invalid ProxyJump host in `{value}`")
        }
        let port = port
            .parse::<u16>()
            .with_context(|| format!("invalid ProxyJump port in `{value}`"))?;
        (host.to_string(), Some(port))
    } else {
        // A raw unbracketed IPv6 literal contains multiple colons and cannot carry
        // an unambiguous port. OpenSSH accepts it as a host-only jump value.
        (authority.to_string(), None)
    };
    if host.is_empty() {
        bail!("empty ProxyJump host in `{value}`")
    }
    Ok(JumpSpec { host, user, port })
}

fn jump_host(all: &[SshHost], value: &str) -> Result<SshHost> {
    let spec = parse_jump_spec(value)?;
    let mut hop = all
        .iter()
        .find(|host| host.alias == spec.host)
        .cloned()
        .map(Ok)
        .unwrap_or_else(|| resolve_effective_host(&spec.host))?;
    if let Some(user) = spec.user {
        hop.user = Some(user);
    }
    if let Some(port) = spec.port {
        hop.port = port;
    }
    hop.proxy_jump.clear();
    Ok(hop)
}

impl KnownHostsHandler {
    fn for_host(host: &SshHost) -> Self {
        let mut paths = host.global_known_hosts_files.clone();
        let user_paths = if host.user_known_hosts_files.is_empty() {
            let home = std::env::var_os("USERPROFILE")
                .or_else(|| std::env::var_os("HOME"))
                .map(PathBuf::from)
                .unwrap_or_default();
            vec![home.join(".ssh").join("known_hosts")]
        } else {
            host.user_known_hosts_files.clone()
        };
        for path in user_paths {
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
        Self {
            lookup_host: host
                .host_key_alias
                .clone()
                .unwrap_or_else(|| host.hostname.clone()),
            port: host.port,
            paths,
        }
    }
}

impl client::Handler for KnownHostsHandler {
    type Error = anyhow::Error;

    async fn check_server_key(
        &mut self,
        key: &russh::keys::PublicKey,
    ) -> Result<bool, Self::Error> {
        // Deliberately stricter than trust-relaxing OpenSSH client modes such as
        // StrictHostKeyChecking=no/accept-new. runwatchd is unattended and only
        // accepts keys already present in effective Global/UserKnownHostsFile.
        for path in &self.paths {
            if !path.is_file() {
                continue;
            }
            let matched = russh::keys::known_hosts::check_known_hosts_path(
                &self.lookup_host,
                self.port,
                key,
                path,
            )
            .with_context(|| {
                format!(
                    "verify SSH host key for {}:{} against {}",
                    self.lookup_host,
                    self.port,
                    path.display()
                )
            })?;
            if matched {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

pub struct ExecOutput {
    pub stdout: String,
    pub stderr: String,
    pub code: Option<u32>,
}

struct LiveSession {
    handle: Handle<KnownHostsHandler>,
}

type SharedSession = Arc<Mutex<LiveSession>>;

pub struct HostPool {
    hosts: Vec<SshHost>,
    sessions: Mutex<HashMap<String, SharedSession>>,
    keepalive: Duration,
    command_timeout: Duration,
}

impl HostPool {
    pub fn from_ssh_config(keepalive: Duration) -> Result<Self> {
        Self::from_ssh_config_with_timeout(keepalive, Duration::from_secs(20))
    }

    pub fn from_ssh_config_with_timeout(
        keepalive: Duration,
        command_timeout: Duration,
    ) -> Result<Self> {
        Ok(Self {
            hosts: parse_ssh_config()?,
            sessions: Mutex::new(HashMap::new()),
            keepalive,
            command_timeout,
        })
    }

    pub fn hosts(&self) -> &[SshHost] {
        &self.hosts
    }

    pub fn resolve(&self, alias: &str) -> Result<&SshHost> {
        self.hosts
            .iter()
            .find(|h| h.alias == alias)
            .with_context(|| format!("Host `{alias}` not in ~/.ssh/config"))
    }

    async fn ensure_session(&self, alias: &str) -> Result<SharedSession> {
        if let Some(existing) = self.sessions.lock().await.get(alias).cloned() {
            return Ok(existing);
        }

        let host = self.resolve(alias)?.clone();
        let handle = connect_chain(&self.hosts, &host, self.keepalive).await?;
        let candidate = Arc::new(Mutex::new(LiveSession { handle }));

        let existing = {
            let mut sessions = self.sessions.lock().await;
            if let Some(existing) = sessions.get(alias).cloned() {
                Some(existing)
            } else {
                sessions.insert(alias.to_string(), candidate.clone());
                None
            }
        };

        if let Some(existing) = existing {
            let candidate = candidate.lock().await;
            let _ = candidate
                .handle
                .disconnect(Disconnect::ByApplication, "duplicate connection", "")
                .await;
            return Ok(existing);
        }

        Ok(candidate)
    }

    async fn remove_if_same(&self, alias: &str, expected: &SharedSession) {
        let removed = {
            let mut sessions = self.sessions.lock().await;
            match sessions.get(alias) {
                Some(current) if Arc::ptr_eq(current, expected) => sessions.remove(alias),
                _ => None,
            }
        };
        if let Some(session) = removed {
            let live = session.lock().await;
            let _ = live
                .handle
                .disconnect(Disconnect::ByApplication, "session invalidated", "")
                .await;
        }
    }

    pub async fn ensure(&self, alias: &str) -> Result<()> {
        self.ensure_session(alias).await.map(|_| ())
    }

    pub async fn exec(&self, alias: &str, command: &str) -> Result<ExecOutput> {
        let session = self.ensure_session(alias).await?;
        let result = {
            let live = session.lock().await;
            tokio::time::timeout(self.command_timeout, exec_on(&live.handle, command)).await
        };

        match result {
            Ok(Ok(out)) => Ok(out),
            Ok(Err(err)) => {
                self.remove_if_same(alias, &session).await;
                Err(err)
            }
            Err(_) => {
                self.remove_if_same(alias, &session).await;
                bail!(
                    "SSH command on {alias} timed out after {:.3}s",
                    self.command_timeout.as_secs_f64()
                )
            }
        }
    }

    pub async fn drop_host(&self, alias: &str) {
        let session = self.sessions.lock().await.remove(alias);
        if let Some(session) = session {
            let live = session.lock().await;
            let _ = live
                .handle
                .disconnect(Disconnect::ByApplication, "", "")
                .await;
        }
    }
}

async fn connect_chain(
    all: &[SshHost],
    target: &SshHost,
    keepalive: Duration,
) -> Result<Handle<KnownHostsHandler>> {
    if target.proxy_jump.is_empty() {
        return connect_direct(target, keepalive).await;
    }
    // First hop is the leftmost jump alias; remaining hops plus the target
    // are reached with direct-tcpip channels (OpenSSH ProxyJump semantics).
    let mut chain: Vec<SshHost> = Vec::new();
    for jump in &target.proxy_jump {
        chain.push(jump_host(all, jump).with_context(|| {
            format!("resolve ProxyJump hop `{jump}` for Host `{}`", target.alias)
        })?);
    }
    chain.push(target.clone());

    let mut handle = connect_direct(&chain[0], keepalive).await?;
    for hop in chain.iter().skip(1) {
        handle = connect_via(&handle, hop, keepalive).await?;
    }
    Ok(handle)
}

fn client_config(keepalive: Duration) -> Arc<client::Config> {
    Arc::new(client::Config {
        inactivity_timeout: Some(Duration::from_secs(600)),
        keepalive_interval: Some(keepalive),
        ..Default::default()
    })
}

async fn connect_direct(host: &SshHost, keepalive: Duration) -> Result<Handle<KnownHostsHandler>> {
    let addr = (host.hostname.as_str(), host.port);
    let mut handle = client::connect(
        client_config(keepalive),
        addr,
        KnownHostsHandler::for_host(host),
    )
    .await
    .with_context(|| format!("connect {}", host.hostname))?;
    authenticate(&mut handle, host).await?;
    Ok(handle)
}

async fn connect_via(
    jump: &Handle<KnownHostsHandler>,
    next: &SshHost,
    keepalive: Duration,
) -> Result<Handle<KnownHostsHandler>> {
    let channel = jump
        .channel_open_direct_tcpip(next.hostname.clone(), next.port.into(), "127.0.0.1", 0)
        .await
        .with_context(|| format!("direct-tcpip {}", next.hostname))?;
    let stream = channel.into_stream();
    let mut handle = client::connect_stream(
        client_config(keepalive),
        stream,
        KnownHostsHandler::for_host(next),
    )
    .await
    .with_context(|| format!("ssh over jump to {}", next.hostname))?;
    authenticate(&mut handle, next).await?;
    Ok(handle)
}

async fn authenticate_agent_client<R>(
    handle: &mut Handle<KnownHostsHandler>,
    user: &str,
    agent: &mut AgentClient<R>,
    allowed_public_keys: Option<&[Vec<u8>]>,
) -> Result<bool>
where
    R: AgentStream + Unpin + Send + 'static,
{
    let identities = agent
        .request_identities()
        .await
        .context("request SSH agent identities")?;
    for key in identities {
        if let Some(allowed) = allowed_public_keys
            && !allowed
                .iter()
                .any(|candidate| candidate == &key.public_key_bytes())
        {
            continue;
        }
        let hash = handle
            .best_supported_rsa_hash()
            .await
            .ok()
            .flatten()
            .flatten();
        let result = handle
            .authenticate_publickey_with(user.to_string(), key, hash, agent)
            .await
            .context("authenticate with SSH agent identity")?;
        if result.success() {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(windows)]
async fn authenticate_system_agent(
    handle: &mut Handle<KnownHostsHandler>,
    user: &str,
    allowed_public_keys: Option<&[Vec<u8>]>,
    failures: &mut Vec<String>,
) -> bool {
    const OPENSSH_AGENT: &str = r"\\.\pipe\openssh-ssh-agent";
    match tokio::time::timeout(
        Duration::from_secs(3),
        AgentClient::connect_named_pipe(OPENSSH_AGENT),
    )
    .await
    {
        Ok(Ok(mut agent)) => match tokio::time::timeout(
            Duration::from_secs(3),
            authenticate_agent_client(handle, user, &mut agent, allowed_public_keys),
        )
        .await
        {
            Ok(Ok(true)) => return true,
            Ok(Ok(false)) => failures.push("Windows OpenSSH agent: no accepted identities".into()),
            Ok(Err(err)) => failures.push(format!("Windows OpenSSH agent: {err:#}")),
            Err(_) => failures.push("Windows OpenSSH agent authentication timed out".into()),
        },
        Ok(Err(err)) => failures.push(format!("Windows OpenSSH agent unavailable: {err}")),
        Err(_) => failures.push("Windows OpenSSH agent connection timed out".into()),
    }

    let mut pageant =
        match tokio::time::timeout(Duration::from_secs(3), AgentClient::connect_pageant()).await {
            Ok(agent) => agent,
            Err(_) => {
                failures.push("Pageant connection timed out".into());
                return false;
            }
        };
    match tokio::time::timeout(
        Duration::from_secs(3),
        authenticate_agent_client(handle, user, &mut pageant, allowed_public_keys),
    )
    .await
    {
        Ok(Ok(true)) => true,
        Ok(Ok(false)) => {
            failures.push("Pageant: no accepted identities".into());
            false
        }
        Ok(Err(err)) => {
            failures.push(format!("Pageant: {err:#}"));
            false
        }
        Err(_) => {
            failures.push("Pageant: timed out".into());
            false
        }
    }
}

#[cfg(unix)]
async fn authenticate_system_agent(
    handle: &mut Handle<KnownHostsHandler>,
    user: &str,
    allowed_public_keys: Option<&[Vec<u8>]>,
    failures: &mut Vec<String>,
) -> bool {
    let mut agent =
        match tokio::time::timeout(Duration::from_secs(3), AgentClient::connect_env()).await {
            Ok(Ok(agent)) => agent,
            Ok(Err(err)) => {
                failures.push(format!("SSH_AUTH_SOCK agent unavailable: {err}"));
                return false;
            }
            Err(_) => {
                failures.push("SSH_AUTH_SOCK agent connection timed out".into());
                return false;
            }
        };
    match tokio::time::timeout(
        Duration::from_secs(3),
        authenticate_agent_client(handle, user, &mut agent, allowed_public_keys),
    )
    .await
    {
        Ok(Ok(true)) => true,
        Ok(Ok(false)) => {
            failures.push("SSH_AUTH_SOCK agent: no accepted identities".into());
            false
        }
        Ok(Err(err)) => {
            failures.push(format!("SSH_AUTH_SOCK agent: {err:#}"));
            false
        }
        Err(_) => {
            failures.push("SSH_AUTH_SOCK agent: timed out".into());
            false
        }
    }
}

fn identity_public_key_bytes(path: &std::path::Path) -> Option<Vec<u8>> {
    if let Ok(public) = load_public_key(path) {
        return Some(public.public_key_bytes());
    }
    let public_path = PathBuf::from(format!("{}.pub", path.display()));
    load_public_key(public_path)
        .ok()
        .map(|public| public.public_key_bytes())
}

async fn authenticate(handle: &mut Handle<KnownHostsHandler>, host: &SshHost) -> Result<()> {
    let user = host
        .user
        .clone()
        .or_else(|| std::env::var("USERNAME").ok())
        .unwrap_or_else(|| "user".into());
    let key_paths = if host.identity_files.is_empty() {
        let home = std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
            .unwrap_or_default();
        vec![home.join(".ssh").join("id_ed25519")]
    } else {
        host.identity_files.clone()
    };

    let mut failures = Vec::new();
    let mut configured_public_keys = Vec::<Vec<u8>>::new();
    let mut encrypted_without_public = Vec::<PathBuf>::new();
    for key_path in &key_paths {
        let key = match load_secret_key(key_path, None) {
            Ok(key) => {
                let bytes = key.public_key().public_key_bytes();
                if !configured_public_keys.contains(&bytes) {
                    configured_public_keys.push(bytes);
                }
                key
            }
            Err(russh::keys::Error::KeyIsEncrypted) => {
                if let Some(bytes) = identity_public_key_bytes(key_path) {
                    if !configured_public_keys.contains(&bytes) {
                        configured_public_keys.push(bytes);
                    }
                    failures.push(format!(
                        "{}: encrypted private key; unattended daemon will use only a matching pre-loaded agent identity",
                        key_path.display()
                    ));
                } else {
                    encrypted_without_public.push(key_path.clone());
                    failures.push(format!(
                        "{}: encrypted private key has no readable matching public identity (.pub); unattended daemon will not prompt for a passphrase",
                        key_path.display()
                    ));
                }
                continue;
            }
            Err(err) => {
                if let Some(bytes) = identity_public_key_bytes(key_path)
                    && !configured_public_keys.contains(&bytes)
                {
                    configured_public_keys.push(bytes);
                }
                failures.push(format!("{}: {err}", key_path.display()));
                continue;
            }
        };
        let hash = handle
            .best_supported_rsa_hash()
            .await
            .ok()
            .flatten()
            .flatten();
        match handle
            .authenticate_publickey(&user, PrivateKeyWithHashAlg::new(Arc::new(key), hash))
            .await
        {
            Ok(ok) if ok.success() => return Ok(()),
            Ok(_) => failures.push(format!("{}: rejected", key_path.display())),
            Err(err) => failures.push(format!("{}: {err}", key_path.display())),
        }
    }

    let allowed_public_keys = host
        .identities_only
        .then_some(configured_public_keys.as_slice());
    if host.identities_only && configured_public_keys.is_empty() {
        failures.push(
            "IdentitiesOnly=yes but no configured IdentityFile yielded a public identity; agent keys cannot be matched safely"
                .into(),
        );
    } else if authenticate_system_agent(handle, &user, allowed_public_keys, &mut failures).await {
        return Ok(());
    }
    if !encrypted_without_public.is_empty() {
        failures.push(
            "encrypted private keys are intentionally non-interactive: preload the key into OpenSSH agent/Pageant and keep a matching public IdentityFile/.pub, or use a non-encrypted service identity"
                .into(),
        );
    }
    bail!(
        "publickey auth failed for {user}@{} using configured identities{} and available SSH agents: {}",
        host.hostname,
        if host.identities_only {
            " (IdentitiesOnly=yes)"
        } else {
            ""
        },
        failures.join("; ")
    )
}

fn apply_channel_message(
    msg: ChannelMsg,
    stdout: &mut Vec<u8>,
    stderr: &mut Vec<u8>,
    code: &mut Option<u32>,
) -> bool {
    match msg {
        ChannelMsg::Data { ref data } => stdout.extend_from_slice(data),
        ChannelMsg::ExtendedData { ref data, ext } if ext == 1 => stderr.extend_from_slice(data),
        ChannelMsg::ExitStatus { exit_status } => *code = Some(exit_status),
        // EOF ends the data stream, not the channel lifecycle. Servers may send
        // exit-status after EOF and before Close, so keep collecting here.
        ChannelMsg::Eof => {}
        ChannelMsg::Close => return true,
        _ => {}
    }
    false
}

async fn exec_on(handle: &Handle<KnownHostsHandler>, command: &str) -> Result<ExecOutput> {
    let mut channel = handle.channel_open_session().await?;
    channel.exec(true, command).await?;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut code = None;
    loop {
        let Some(msg) = channel.wait().await else {
            break;
        };
        if apply_channel_message(msg, &mut stdout, &mut stderr, &mut code) {
            break;
        }
    }
    Ok(ExecOutput {
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        code,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use russh::client::Handler;

    #[test]
    fn proxy_jump_parser_handles_alias_user_port_and_ipv6() {
        assert_eq!(
            parse_jump_spec("gate").unwrap(),
            JumpSpec {
                host: "gate".into(),
                user: None,
                port: None,
            }
        );
        assert_eq!(
            parse_jump_spec("alice@gate:2200").unwrap(),
            JumpSpec {
                host: "gate".into(),
                user: Some("alice".into()),
                port: Some(2200),
            }
        );
        assert_eq!(
            parse_jump_spec("alice@[2001:db8::1]:2201").unwrap(),
            JumpSpec {
                host: "2001:db8::1".into(),
                user: Some("alice".into()),
                port: Some(2201),
            }
        );
        assert_eq!(
            parse_jump_spec("2001:db8::2").unwrap(),
            JumpSpec {
                host: "2001:db8::2".into(),
                user: None,
                port: None,
            }
        );
        assert!(parse_jump_spec("gate:not-a-port").is_err());
        assert!(parse_jump_spec("[2001:db8::1").is_err());
    }

    #[test]
    fn known_hosts_handler_combines_global_and_user_paths_without_duplicates() {
        let mut host = SshHost {
            alias: "cluster".into(),
            hostname: "cluster.example".into(),
            user: Some("scientist".into()),
            port: 22,
            identity_files: Vec::new(),
            identities_only: false,
            global_known_hosts_files: vec![PathBuf::from("global"), PathBuf::from("shared")],
            user_known_hosts_files: vec![PathBuf::from("shared"), PathBuf::from("user")],
            host_key_alias: None,
            proxy_jump: Vec::new(),
        };
        let handler = KnownHostsHandler::for_host(&host);
        assert_eq!(
            handler.paths,
            vec![
                PathBuf::from("global"),
                PathBuf::from("shared"),
                PathBuf::from("user")
            ]
        );
        host.host_key_alias = Some("stable".into());
        assert_eq!(KnownHostsHandler::for_host(&host).lookup_host, "stable");
    }

    #[test]
    fn configured_public_identity_can_be_read_from_adjacent_pub_without_private_key_access() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("runwatch-agent-pub-{nonce}"));
        std::fs::create_dir_all(&dir).unwrap();
        let private = dir.join("id_example");
        let public = dir.join("id_example.pub");
        std::fs::write(
            &public,
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ test\n",
        )
        .unwrap();
        assert!(identity_public_key_bytes(&private).is_some());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn missing_known_hosts_never_auto_trusts_a_server_key() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("runwatch-known-hosts-strict-{nonce}"));
        std::fs::create_dir_all(&dir).unwrap();
        let public_path = dir.join("server.pub");
        std::fs::write(
            &public_path,
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ server\n",
        )
        .unwrap();
        let key = load_public_key(&public_path).unwrap();
        let mut handler = KnownHostsHandler {
            lookup_host: "new.example".into(),
            port: 22,
            paths: vec![dir.join("does-not-exist-known-hosts")],
        };
        assert!(!handler.check_server_key(&key).await.unwrap());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn eof_does_not_hide_later_exit_status() {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut code = None;
        assert!(!apply_channel_message(
            ChannelMsg::Eof,
            &mut stdout,
            &mut stderr,
            &mut code,
        ));
        assert!(!apply_channel_message(
            ChannelMsg::ExitStatus { exit_status: 0 },
            &mut stdout,
            &mut stderr,
            &mut code,
        ));
        assert_eq!(code, Some(0));
        assert!(apply_channel_message(
            ChannelMsg::Close,
            &mut stdout,
            &mut stderr,
            &mut code,
        ));
    }
}
