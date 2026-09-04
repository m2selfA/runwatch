use anyhow::{Context, Result};
use glob::glob;
use std::collections::{BTreeSet, HashSet};
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

#[derive(Debug, Clone)]
pub struct SshHost {
    pub alias: String,
    pub hostname: String,
    pub user: Option<String>,
    pub port: u16,
    pub identity_files: Vec<PathBuf>,
    pub identities_only: bool,
    pub global_known_hosts_files: Vec<PathBuf>,
    pub user_known_hosts_files: Vec<PathBuf>,
    pub host_key_alias: Option<String>,
    pub proxy_jump: Vec<String>,
}

impl SshHost {
    fn new(alias: &str) -> Self {
        Self {
            alias: alias.to_string(),
            hostname: alias.to_string(),
            user: None,
            port: 22,
            identity_files: Vec::new(),
            identities_only: false,
            global_known_hosts_files: Vec::new(),
            user_known_hosts_files: Vec::new(),
            host_key_alias: None,
            proxy_jump: Vec::new(),
        }
    }
}

fn ssh_config_path_from(
    override_path: Option<OsString>,
    userprofile: Option<OsString>,
    home: Option<OsString>,
) -> PathBuf {
    if let Some(path) = override_path.filter(|value| !value.is_empty()) {
        return PathBuf::from(path);
    }
    if let Some(home) = userprofile.or(home) {
        return PathBuf::from(home).join(".ssh").join("config");
    }
    PathBuf::from(".ssh/config")
}

pub fn ssh_config_path() -> PathBuf {
    ssh_config_path_from(
        std::env::var_os("RUNWATCH_SSH_CONFIG"),
        std::env::var_os("USERPROFILE"),
        std::env::var_os("HOME"),
    )
}

fn ssh_g_args_from(alias: &str, override_path: Option<OsString>) -> Vec<OsString> {
    let mut args = Vec::new();
    if let Some(path) = override_path.filter(|value| !value.is_empty()) {
        args.push(OsString::from("-F"));
        args.push(path);
    }
    args.push(OsString::from("-G"));
    args.push(OsString::from(alias));
    args
}

fn ssh_g_args(alias: &str) -> Vec<OsString> {
    ssh_g_args_from(alias, std::env::var_os("RUNWATCH_SSH_CONFIG"))
}

fn openssh_command() -> Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        // `runwatch-gui` is a Windows-subsystem process with no parent console.
        // Without this flag every `ssh -G` config probe allocates a transient
        // console window. stdout/stderr are still captured by `output()`, so
        // CLI diagnostics remain fully available through the returned error.
        let mut command = Command::new("ssh");
        command.creation_flags(CREATE_NO_WINDOW);
        command
    }
    #[cfg(not(windows))]
    {
        Command::new("ssh")
    }
}

pub fn parse_ssh_config() -> Result<Vec<SshHost>> {
    let declared = parse_ssh_config_at(&ssh_config_path())?;
    declared
        .into_iter()
        .map(|fallback| {
            resolve_effective_host(&fallback.alias).with_context(|| {
                format!(
                    "resolve effective OpenSSH configuration for Host `{}` with `ssh -G`",
                    fallback.alias
                )
            })
        })
        .collect()
}

pub(crate) fn resolve_effective_host(alias: &str) -> Result<SshHost> {
    let output = openssh_command()
        .args(ssh_g_args(alias))
        .output()
        .with_context(|| format!("run `ssh -G {alias}`"))?;
    if !output.status.success() {
        anyhow::bail!(
            "`ssh -G {alias}` exited {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let stdout = String::from_utf8(output.stdout).context("OpenSSH -G output is not UTF-8")?;
    parse_effective_config(alias, &stdout)
}

fn parse_effective_config(alias: &str, text: &str) -> Result<SshHost> {
    let mut host = SshHost::new(alias);
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(2, char::is_whitespace);
        let key = parts.next().unwrap_or_default().to_ascii_lowercase();
        let value = parts.next().unwrap_or_default().trim();
        match key.as_str() {
            "hostname" if !value.is_empty() => host.hostname = value.to_string(),
            "user" if !value.is_empty() => host.user = Some(value.to_string()),
            "port" => {
                host.port = value.parse().with_context(|| {
                    format!("invalid OpenSSH port {value:?} for Host `{alias}`")
                })?;
            }
            "identityfile" if !value.is_empty() && !value.eq_ignore_ascii_case("none") => {
                let path = expand_tilde(value);
                if !host.identity_files.contains(&path) {
                    host.identity_files.push(path);
                }
            }
            "identitiesonly" => {
                host.identities_only = value.eq_ignore_ascii_case("yes");
            }
            "globalknownhostsfile" if !value.is_empty() && !value.eq_ignore_ascii_case("none") => {
                for path in value.split_whitespace().map(expand_openssh_path) {
                    if !host.global_known_hosts_files.contains(&path) {
                        host.global_known_hosts_files.push(path);
                    }
                }
            }
            "userknownhostsfile" if !value.is_empty() && !value.eq_ignore_ascii_case("none") => {
                for path in value.split_whitespace().map(expand_openssh_path) {
                    if !host.user_known_hosts_files.contains(&path) {
                        host.user_known_hosts_files.push(path);
                    }
                }
            }
            "hostkeyalias" if !value.is_empty() && !value.eq_ignore_ascii_case("none") => {
                host.host_key_alias = Some(value.to_string());
            }
            "proxyjump" if !value.is_empty() && !value.eq_ignore_ascii_case("none") => {
                host.proxy_jump = value
                    .split(',')
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
                    .collect();
            }
            _ => {}
        }
    }
    if host.hostname.trim().is_empty() {
        anyhow::bail!("OpenSSH -G returned no HostName for Host `{alias}`");
    }
    Ok(host)
}

pub fn parse_ssh_config_at(path: &std::path::Path) -> Result<Vec<SshHost>> {
    let mut aliases = BTreeSet::new();
    let mut visited = HashSet::new();
    collect_declared_aliases(path, &mut aliases, &mut visited)?;
    Ok(aliases
        .into_iter()
        .map(|alias| SshHost::new(&alias))
        .collect())
}

fn collect_declared_aliases(
    path: &std::path::Path,
    aliases: &mut BTreeSet<String>,
    visited: &mut HashSet<PathBuf>,
) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let identity = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if !visited.insert(identity) {
        return Ok(());
    }
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));

    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(2, char::is_whitespace);
        let key = parts.next().unwrap_or("");
        let value = parts.next().unwrap_or("").trim();
        if key.eq_ignore_ascii_case("Host") {
            for alias in value.split_whitespace().filter(|alias| {
                !alias.is_empty()
                    && !alias.starts_with('!')
                    && !alias.contains('*')
                    && !alias.contains('?')
                    && !alias.contains('[')
            }) {
                aliases.insert(alias.to_string());
            }
        } else if key.eq_ignore_ascii_case("Include") {
            for token in value.split_whitespace() {
                let token = token.trim_matches(['\'', '"']);
                if token.is_empty() {
                    continue;
                }
                let expanded = expand_tilde(token);
                let pattern_path = if expanded.is_absolute() {
                    expanded
                } else {
                    parent.join(expanded)
                };
                let pattern = pattern_path.to_string_lossy().replace('\\', "/");
                let matches = glob(&pattern)
                    .with_context(|| format!("invalid OpenSSH Include pattern `{token}`"))?;
                for entry in matches {
                    let included = entry.with_context(|| {
                        format!(
                            "expand OpenSSH Include pattern `{token}` from {}",
                            path.display()
                        )
                    })?;
                    collect_declared_aliases(&included, aliases, visited)?;
                }
            }
        }
    }
    Ok(())
}

fn expand_openssh_path(value: &str) -> PathBuf {
    #[cfg(windows)]
    if let Some(rest) = value.strip_prefix("__PROGRAMDATA__")
        && let Some(program_data) = std::env::var_os("PROGRAMDATA")
    {
        return PathBuf::from(program_data).join(rest.trim_start_matches(['/', '\\']));
    }
    expand_tilde(value)
}

fn expand_tilde(value: &str) -> PathBuf {
    if let Some(rest) = value.strip_prefix('~') {
        if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
            return PathBuf::from(home).join(rest.trim_start_matches(['/', '\\']));
        }
    }
    PathBuf::from(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_config_override_is_explicit_and_does_not_replace_home_semantics() {
        assert_eq!(
            ssh_config_path_from(
                Some(OsString::from("C:/isolated/ssh-config")),
                Some(OsString::from("C:/Users/real")),
                None,
            ),
            PathBuf::from("C:/isolated/ssh-config")
        );
        assert_eq!(
            ssh_config_path_from(None, Some(OsString::from("C:/Users/real")), None),
            PathBuf::from("C:/Users/real").join(".ssh").join("config")
        );
    }

    #[test]
    fn ssh_g_override_uses_the_same_explicit_config_source() {
        assert_eq!(
            ssh_g_args_from("gm00", Some(OsString::from("C:/isolated/ssh-config"))),
            vec![
                OsString::from("-F"),
                OsString::from("C:/isolated/ssh-config"),
                OsString::from("-G"),
                OsString::from("gm00"),
            ]
        );
        assert_eq!(
            ssh_g_args_from("gm00", None),
            vec![OsString::from("-G"), OsString::from("gm00")]
        );
    }

    #[test]
    fn discovers_exact_hosts_through_recursive_includes_without_wildcard_aliases() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("runwatch-ssh-include-{nonce}"));
        let includes = dir.join("config.d");
        fs::create_dir_all(&includes).unwrap();
        let root = dir.join("config");
        let nested = includes.join("nested.conf");
        let first = includes.join("10-cluster.conf");
        fs::write(
            &root,
            "Host direct\n  HostName direct.example\nInclude config.d/*.conf\n",
        )
        .unwrap();
        fs::write(
            &first,
            "Host cluster wildcard* !blocked exact-two\n  ProxyJump jump\nInclude nested.conf\n",
        )
        .unwrap();
        fs::write(&nested, "Host deep host? bracket[ab]\nInclude ../config\n").unwrap();

        let hosts = parse_ssh_config_at(&root).unwrap();
        let aliases = hosts.into_iter().map(|host| host.alias).collect::<Vec<_>>();
        assert_eq!(aliases, vec!["cluster", "deep", "direct", "exact-two"]);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn parses_effective_ssh_g_output_with_multiple_identities() {
        let host = parse_effective_config(
            "cluster",
            "host cluster\nuser scientist\nhostname login.example\nport 2222\nidentityfile ~/.ssh/id_ed25519\nidentityfile ~/.ssh/id_rsa\nidentitiesonly yes\nglobalknownhostsfile __PROGRAMDATA__\\ssh/ssh_known_hosts __PROGRAMDATA__\\ssh/ssh_known_hosts2\nuserknownhostsfile ~/.ssh/known_hosts ~/.ssh/known_hosts2\nhostkeyalias stable-login\nproxyjump gate,gate2\n",
        )
        .unwrap();
        assert_eq!(host.alias, "cluster");
        assert_eq!(host.hostname, "login.example");
        assert_eq!(host.user.as_deref(), Some("scientist"));
        assert_eq!(host.port, 2222);
        assert_eq!(host.identity_files.len(), 2);
        assert!(host.identities_only);
        assert_eq!(host.global_known_hosts_files.len(), 2);
        #[cfg(windows)]
        if std::env::var_os("PROGRAMDATA").is_some() {
            assert!(
                host.global_known_hosts_files
                    .iter()
                    .all(|path| !path.to_string_lossy().contains("__PROGRAMDATA__"))
            );
        }
        assert_eq!(host.user_known_hosts_files.len(), 2);
        assert_eq!(host.host_key_alias.as_deref(), Some("stable-login"));
        assert_eq!(host.proxy_jump, vec!["gate", "gate2"]);
    }
}
