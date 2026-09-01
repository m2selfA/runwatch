use anyhow::{Context, Result, bail};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

#[cfg(windows)]
use std::os::windows::process::CommandExt;
#[cfg(windows)]
use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

const MCP_NAME: &str = "runwatch";

#[derive(Debug, Clone, PartialEq, Eq)]
struct McpEntry {
    enabled: bool,
    transport: String,
    command: Option<String>,
    args: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RegistrationState {
    Missing,
    Owned(McpEntry),
    Conflict(McpEntry),
}

fn codex_program() -> OsString {
    if let Some(value) = std::env::var_os("RUNWATCH_CODEX_EXECUTABLE")
        && !value.is_empty()
    {
        return value;
    }
    #[cfg(windows)]
    {
        OsString::from("codex.exe")
    }
    #[cfg(not(windows))]
    {
        OsString::from("codex")
    }
}

fn run_codex<I, S>(args: I) -> Result<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let program = codex_program();
    let mut command = Command::new(&program);
    command.args(args).stdin(Stdio::null());
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    command
        .output()
        .with_context(|| format!("launch {}", PathBuf::from(&program).display()))
}

fn mcp_file_name() -> &'static str {
    #[cfg(windows)]
    {
        "runwatch-mcp.exe"
    }
    #[cfg(not(windows))]
    {
        "runwatch-mcp"
    }
}

fn sibling_mcp_candidate() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("resolve current runwatch executable")?;
    let dir = exe
        .parent()
        .context("runwatch executable has no parent directory")?;
    Ok(dir.join(mcp_file_name()))
}

fn consumer_safe_path(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let value = path.to_string_lossy();
        if let Some(rest) = value.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = value.strip_prefix(r"\\?\") {
            return PathBuf::from(rest);
        }
    }
    path.to_path_buf()
}

fn sibling_mcp_for_install() -> Result<PathBuf> {
    let candidate = sibling_mcp_candidate()?;
    if !candidate.is_file() {
        bail!(
            "runwatch-mcp is not installed beside runwatch: {}",
            candidate.display()
        );
    }
    let canonical = candidate
        .canonicalize()
        .with_context(|| format!("resolve runwatch-mcp path {}", candidate.display()))?;
    Ok(consumer_safe_path(&canonical))
}

fn parse_mcp_get(stdout: &str) -> Result<McpEntry> {
    let mut enabled = None;
    let mut transport = None;
    let mut command = None;
    let mut args = None;
    for line in stdout.lines().skip(1) {
        let line = line.trim();
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "enabled" => enabled = Some(value == "true"),
            "transport" => transport = Some(value.to_string()),
            "command" if value != "-" => command = Some(value.to_string()),
            "args" if value != "-" => args = Some(value.to_string()),
            _ => {}
        }
    }
    Ok(McpEntry {
        enabled: enabled.context("Codex MCP entry omitted enabled")?,
        transport: transport.context("Codex MCP entry omitted transport")?,
        command,
        args,
    })
}

fn missing_mcp(stderr: &str) -> bool {
    stderr.contains("No MCP server named 'runwatch' found")
}

fn normalized_path_text(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    #[cfg(windows)]
    {
        text.to_ascii_lowercase()
    }
    #[cfg(not(windows))]
    {
        text
    }
}

fn configured_command_is_owned(command: &str, expected: &Path) -> bool {
    let configured = PathBuf::from(command);
    let configured = configured.canonicalize().unwrap_or(configured);
    let expected = expected
        .canonicalize()
        .unwrap_or_else(|_| expected.to_path_buf());
    normalized_path_text(&consumer_safe_path(&configured))
        == normalized_path_text(&consumer_safe_path(&expected))
}

fn classify_registration(
    success: bool,
    stdout: &str,
    stderr: &str,
    expected: &Path,
) -> Result<RegistrationState> {
    if !success {
        if missing_mcp(stderr) {
            return Ok(RegistrationState::Missing);
        }
        bail!("codex mcp get {MCP_NAME} failed: {}", stderr.trim());
    }
    let entry = parse_mcp_get(stdout)?;
    let owned = entry.transport == "stdio"
        && entry.args.is_none()
        && entry
            .command
            .as_deref()
            .is_some_and(|command| configured_command_is_owned(command, expected));
    Ok(if owned {
        RegistrationState::Owned(entry)
    } else {
        RegistrationState::Conflict(entry)
    })
}

fn registration_state(expected: &Path) -> Result<RegistrationState> {
    let output = run_codex(["mcp", "get", MCP_NAME])?;
    classify_registration(
        output.status.success(),
        &String::from_utf8_lossy(&output.stdout),
        &String::from_utf8_lossy(&output.stderr),
        expected,
    )
}

fn codex_version() -> Result<String> {
    let output = run_codex(["--version"])?;
    if !output.status.success() {
        bail!(
            "codex --version failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn require_success(output: Output, action: &str) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }
    bail!(
        "{action} failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn conflict_message(entry: &McpEntry, expected: &Path) -> String {
    format!(
        "Codex MCP name '{MCP_NAME}' is already owned by another configuration (transport={}, command={}, args={}); refusing to overwrite or remove it. Expected command: {}",
        entry.transport,
        entry.command.as_deref().unwrap_or("-"),
        entry.args.as_deref().unwrap_or("-"),
        expected.display()
    )
}

pub fn status() -> Result<()> {
    let expected = sibling_mcp_candidate()?;
    match codex_version() {
        Ok(version) => println!("codex=available version={version}"),
        Err(error) => {
            println!("codex=unavailable error={error}");
            println!("mcp_expected={}", expected.display());
            return Ok(());
        }
    }
    println!(
        "mcp_binary={} path={}",
        if expected.is_file() {
            "available"
        } else {
            "missing"
        },
        expected.display()
    );
    match registration_state(&expected)? {
        RegistrationState::Missing => println!("registration=missing name={MCP_NAME}"),
        RegistrationState::Owned(entry) => println!(
            "registration=installed name={MCP_NAME} enabled={} command={}",
            entry.enabled,
            entry.command.as_deref().unwrap_or("-")
        ),
        RegistrationState::Conflict(entry) => println!(
            "registration=conflict name={MCP_NAME} transport={} command={} args={}",
            entry.transport,
            entry.command.as_deref().unwrap_or("-"),
            entry.args.as_deref().unwrap_or("-")
        ),
    }
    Ok(())
}

pub fn install() -> Result<()> {
    let expected = sibling_mcp_for_install()?;
    let version = codex_version()?;
    match registration_state(&expected)? {
        RegistrationState::Owned(_) => {
            println!(
                "already installed Codex MCP {MCP_NAME} -> {} ({version})",
                expected.display()
            );
            return Ok(());
        }
        RegistrationState::Conflict(entry) => bail!("{}", conflict_message(&entry, &expected)),
        RegistrationState::Missing => {}
    }

    let expected_arg = expected.as_os_str().to_os_string();
    require_success(
        run_codex([
            OsString::from("mcp"),
            OsString::from("add"),
            OsString::from(MCP_NAME),
            OsString::from("--"),
            expected_arg,
        ])?,
        "register runwatch Codex MCP",
    )?;
    match registration_state(&expected)? {
        RegistrationState::Owned(_) => {
            println!("installed Codex MCP {MCP_NAME} -> {}", expected.display());
            Ok(())
        }
        RegistrationState::Missing => bail!("Codex MCP registration disappeared after install"),
        RegistrationState::Conflict(entry) => bail!(
            "Codex MCP registration did not round-trip to the expected path: {}",
            conflict_message(&entry, &expected)
        ),
    }
}

pub fn remove() -> Result<()> {
    let expected = sibling_mcp_candidate()?;
    let _ = codex_version()?;
    match registration_state(&expected)? {
        RegistrationState::Missing => {
            println!("not installed Codex MCP {MCP_NAME}");
            return Ok(());
        }
        RegistrationState::Conflict(entry) => bail!("{}", conflict_message(&entry, &expected)),
        RegistrationState::Owned(_) => {}
    }

    require_success(
        run_codex(["mcp", "remove", MCP_NAME])?,
        "remove runwatch Codex MCP",
    )?;
    match registration_state(&expected)? {
        RegistrationState::Missing => {
            println!("removed Codex MCP {MCP_NAME}");
            Ok(())
        }
        RegistrationState::Owned(_) | RegistrationState::Conflict(_) => {
            bail!("Codex MCP {MCP_NAME} still exists after remove")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_codex_mcp_get_and_classifies_owned_entry() {
        let expected = if cfg!(windows) {
            PathBuf::from(r"C:\Program Files\runwatch\runwatch-mcp.exe")
        } else {
            PathBuf::from("/opt/runwatch/runwatch-mcp")
        };
        let command = if cfg!(windows) {
            "c:/program files/runwatch/runwatch-mcp.exe"
        } else {
            "/opt/runwatch/runwatch-mcp"
        };
        let stdout = format!(
            "runwatch\n  enabled: true\n  transport: stdio\n  command: {command}\n  args: -\n  cwd: -\n  env: -\n  remove: codex mcp remove runwatch\n"
        );
        let state = classify_registration(true, &stdout, "", &expected).unwrap();
        assert!(matches!(state, RegistrationState::Owned(_)));
    }

    #[test]
    fn missing_and_conflicting_entries_fail_closed() {
        let expected = PathBuf::from(if cfg!(windows) {
            r"C:\runwatch\runwatch-mcp.exe"
        } else {
            "/opt/runwatch/runwatch-mcp"
        });
        assert_eq!(
            classify_registration(
                false,
                "",
                "Error: No MCP server named 'runwatch' found.\n",
                &expected,
            )
            .unwrap(),
            RegistrationState::Missing
        );

        let stdout = "runwatch\n  enabled: true\n  transport: stdio\n  command: other-mcp\n  args: serve\n  cwd: -\n  env: -\n";
        let state = classify_registration(true, stdout, "", &expected).unwrap();
        assert!(matches!(state, RegistrationState::Conflict(_)));
    }

    #[cfg(windows)]
    #[test]
    fn windows_verbatim_paths_are_not_written_to_codex_config() {
        assert_eq!(
            consumer_safe_path(Path::new(r"\\?\C:\Tools\runwatch-mcp.exe")),
            PathBuf::from(r"C:\Tools\runwatch-mcp.exe")
        );
        assert_eq!(
            consumer_safe_path(Path::new(r"\\?\UNC\server\share\runwatch-mcp.exe")),
            PathBuf::from(r"\\server\share\runwatch-mcp.exe")
        );
    }

    #[test]
    fn malformed_success_output_is_not_treated_as_missing() {
        let expected = PathBuf::from("runwatch-mcp");
        let error = classify_registration(true, "runwatch\n", "", &expected).unwrap_err();
        assert!(error.to_string().contains("omitted enabled"));
    }
}
