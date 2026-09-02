use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, DateTime, ZipArchive, ZipWriter};

const REQUIRED_BINARIES: [&str; 3] = ["runwatch", "runwatch-mcp", "runwatch-gui"];
const MANIFEST_NAME: &str = "release-manifest.json";
const SUMS_NAME: &str = "SHA256SUMS.txt";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ManifestFile {
    path: String,
    bytes: u64,
    sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ReleaseLayout {
    runwatch: String,
    runwatch_mcp: String,
    runwatch_gui: String,
    sibling_binaries_required: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ReleaseManifest {
    schema_version: u32,
    package: String,
    version: String,
    platform: String,
    profile: String,
    layout: ReleaseLayout,
    files: Vec<ManifestFile>,
}

#[derive(Debug, Clone)]
struct SourceEntry {
    path: String,
    source: PathBuf,
    bytes: u64,
    sha256: String,
    mode: u32,
}

#[derive(Debug, Clone)]
enum ArchivePayload {
    Source(PathBuf),
    Bytes(Vec<u8>),
}

#[derive(Debug, Clone)]
struct ArchiveEntry {
    path: String,
    payload: ArchivePayload,
    mode: u32,
}

#[derive(Debug, Clone)]
struct PackageOptions {
    output_dir: PathBuf,
    target_dir: PathBuf,
    skip_build: bool,
}

fn main() {
    if let Err(error) = real_main() {
        eprintln!("xtask error: {error:#}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    let mut args = env::args_os();
    let _program = args.next();
    let Some(command) = args.next() else {
        print_usage();
        bail!("missing command");
    };
    match command.to_string_lossy().as_ref() {
        "package" => package_command(args.collect()),
        "verify" => verify_command(args.collect()),
        "help" | "--help" | "-h" => {
            print_usage();
            Ok(())
        }
        other => {
            print_usage();
            bail!("unknown command {other:?}");
        }
    }
}

fn print_usage() {
    eprintln!(
        "runwatch xtask\n\n  cargo run -p xtask -- package [--output-dir DIR] [--target-dir DIR] [--skip-build]\n  cargo run -p xtask -- verify ARCHIVE.zip"
    );
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives directly under workspace root")
        .to_path_buf()
}

fn absolute_from(root: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        root.join(path)
    }
}

fn default_target_dir(root: &Path) -> PathBuf {
    env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .map(|path| absolute_from(root, path))
        .unwrap_or_else(|| root.join("target"))
}

fn parse_package_options(args: Vec<std::ffi::OsString>) -> Result<PackageOptions> {
    let root = workspace_root();
    let mut output_dir = root.join("dist");
    let mut target_dir = default_target_dir(&root);
    let mut skip_build = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].to_string_lossy().as_ref() {
            "--output-dir" => {
                index += 1;
                let value = args.get(index).context("--output-dir requires a path")?;
                output_dir = absolute_from(&root, PathBuf::from(value));
            }
            "--target-dir" => {
                index += 1;
                let value = args.get(index).context("--target-dir requires a path")?;
                target_dir = absolute_from(&root, PathBuf::from(value));
            }
            "--skip-build" => skip_build = true,
            other => bail!("unknown package argument {other:?}"),
        }
        index += 1;
    }
    Ok(PackageOptions {
        output_dir,
        target_dir,
        skip_build,
    })
}

fn native_name(stem: &str) -> String {
    if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem.to_owned()
    }
}

fn platform_tag() -> String {
    format!("{}-{}", env::consts::OS, env::consts::ARCH)
}

fn package_name() -> String {
    format!("runwatch-v{}-{}", env!("CARGO_PKG_VERSION"), platform_tag())
}

fn build_release(root: &Path, target_dir: &Path) -> Result<()> {
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(cargo)
        .current_dir(root)
        .args(["build", "--release", "--target-dir"])
        .arg(target_dir)
        .args([
            "-p",
            "runwatch-cli",
            "-p",
            "runwatch-mcp",
            "-p",
            "runwatch-gui",
        ])
        .status()
        .context("launch cargo build for release package")?;
    if !status.success() {
        bail!("release cargo build failed with {status}");
    }
    Ok(())
}

fn sha256_reader(mut reader: impl Read) -> Result<String> {
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(hex_digest(digest.finalize().as_slice()))
}

fn sha256_file(path: &Path) -> Result<String> {
    sha256_reader(File::open(path).with_context(|| format!("open {}", path.display()))?)
}

fn sha256_bytes(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex_digest(digest.as_slice())
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut output, "{byte:02x}").expect("write to String cannot fail");
    }
    output
}

fn source_entry(path: impl Into<String>, source: PathBuf, mode: u32) -> Result<SourceEntry> {
    if !source.is_file() {
        bail!("release payload is missing: {}", source.display());
    }
    let bytes = source
        .metadata()
        .with_context(|| format!("stat {}", source.display()))?
        .len();
    let sha256 = sha256_file(&source)?;
    Ok(SourceEntry {
        path: path.into(),
        source,
        bytes,
        sha256,
        mode,
    })
}

fn collect_payload(root: &Path, target_dir: &Path) -> Result<Vec<SourceEntry>> {
    let release_dir = target_dir.join("release");
    let mut entries = Vec::new();
    for stem in REQUIRED_BINARIES {
        let name = native_name(stem);
        entries.push(source_entry(name.clone(), release_dir.join(&name), 0o755)?);
    }
    entries.push(source_entry("README.md", root.join("README.md"), 0o644)?);
    entries.push(source_entry(
        "docs/INSTALL.md",
        root.join("docs/INSTALL.md"),
        0o644,
    )?);
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(entries)
}

fn render_manifest(entries: &[SourceEntry]) -> Result<Vec<u8>> {
    let manifest = ReleaseManifest {
        schema_version: 1,
        package: "runwatch".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        platform: platform_tag(),
        profile: "release".into(),
        layout: ReleaseLayout {
            runwatch: native_name("runwatch"),
            runwatch_mcp: native_name("runwatch-mcp"),
            runwatch_gui: native_name("runwatch-gui"),
            sibling_binaries_required: true,
        },
        files: entries
            .iter()
            .map(|entry| ManifestFile {
                path: entry.path.clone(),
                bytes: entry.bytes,
                sha256: entry.sha256.clone(),
            })
            .collect(),
    };
    let mut bytes = serde_json::to_vec_pretty(&manifest)?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn render_sums(entries: &[SourceEntry], manifest: &[u8]) -> Vec<u8> {
    let mut rows = entries
        .iter()
        .map(|entry| (entry.path.clone(), entry.sha256.clone()))
        .collect::<Vec<_>>();
    rows.push((MANIFEST_NAME.into(), sha256_bytes(manifest)));
    rows.sort_by(|left, right| left.0.cmp(&right.0));
    let mut output = String::new();
    for (path, hash) in rows {
        output.push_str(&hash);
        output.push_str("  ");
        output.push_str(&path);
        output.push('\n');
    }
    output.into_bytes()
}

fn archive_entries(entries: &[SourceEntry], manifest: Vec<u8>, sums: Vec<u8>) -> Vec<ArchiveEntry> {
    let mut result = entries
        .iter()
        .map(|entry| ArchiveEntry {
            path: entry.path.clone(),
            payload: ArchivePayload::Source(entry.source.clone()),
            mode: entry.mode,
        })
        .collect::<Vec<_>>();
    result.push(ArchiveEntry {
        path: MANIFEST_NAME.into(),
        payload: ArchivePayload::Bytes(manifest),
        mode: 0o644,
    });
    result.push(ArchiveEntry {
        path: SUMS_NAME.into(),
        payload: ArchivePayload::Bytes(sums),
        mode: 0o644,
    });
    result.sort_by(|left, right| left.path.cmp(&right.path));
    result
}

fn write_archive(final_path: &Path, root_name: &str, entries: &[ArchiveEntry]) -> Result<()> {
    if final_path.exists() {
        bail!(
            "refusing to overwrite existing release artifact {}; move it aside or to the Recycle Bin first",
            final_path.display()
        );
    }
    let parent = final_path
        .parent()
        .context("release archive has no parent directory")?;
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let file_name = final_path
        .file_name()
        .context("release archive has no file name")?
        .to_string_lossy();
    let partial = parent.join(format!(".{file_name}.partial-{}", std::process::id()));
    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&partial)
        .with_context(|| format!("create partial archive {}", partial.display()))?;
    let mut zip = ZipWriter::new(file);
    for entry in entries {
        let options = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .last_modified_time(DateTime::default())
            .unix_permissions(entry.mode);
        let name = format!("{root_name}/{}", entry.path.replace('\\', "/"));
        zip.start_file(name, options)?;
        match &entry.payload {
            ArchivePayload::Source(path) => {
                let mut source =
                    File::open(path).with_context(|| format!("open payload {}", path.display()))?;
                std::io::copy(&mut source, &mut zip)?;
            }
            ArchivePayload::Bytes(bytes) => zip.write_all(bytes)?,
        }
    }
    let mut file = zip.finish()?;
    file.flush()?;
    file.sync_all()?;
    drop(file);
    fs::rename(&partial, final_path).with_context(|| {
        format!(
            "promote completed archive {} -> {}",
            partial.display(),
            final_path.display()
        )
    })?;
    Ok(())
}

fn package_command(args: Vec<std::ffi::OsString>) -> Result<()> {
    let options = parse_package_options(args)?;
    let root = workspace_root();
    if !options.skip_build {
        build_release(&root, &options.target_dir)?;
    }
    let payload = collect_payload(&root, &options.target_dir)?;
    let manifest = render_manifest(&payload)?;
    let sums = render_sums(&payload, &manifest);
    let entries = archive_entries(&payload, manifest, sums);
    let name = package_name();
    let archive = options.output_dir.join(format!("{name}.zip"));
    write_archive(&archive, &name, &entries)?;
    let archive_hash = sha256_file(&archive)?;
    println!(
        "{}",
        serde_json::json!({
            "ok": true,
            "package": name,
            "archive": archive,
            "archive_bytes": archive.metadata()?.len(),
            "archive_sha256": archive_hash,
        })
    );
    Ok(())
}

fn verify_command(args: Vec<std::ffi::OsString>) -> Result<()> {
    if args.len() != 1 {
        bail!("verify requires exactly one archive path");
    }
    let archive = absolute_from(&workspace_root(), PathBuf::from(&args[0]));
    let manifest = verify_archive(&archive)?;
    println!(
        "{}",
        serde_json::json!({
            "ok": true,
            "archive": archive,
            "version": manifest.version,
            "platform": manifest.platform,
            "files": manifest.files.len(),
            "archive_sha256": sha256_file(&archive)?,
        })
    );
    Ok(())
}

fn read_zip_entry(archive: &mut ZipArchive<File>, name: &str) -> Result<Vec<u8>> {
    let mut entry = archive
        .by_name(name)
        .with_context(|| format!("archive is missing {name}"))?;
    let mut bytes = Vec::new();
    entry.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn parse_sums(bytes: &[u8]) -> Result<BTreeMap<String, String>> {
    let text = std::str::from_utf8(bytes).context("SHA256SUMS.txt is not UTF-8")?;
    let mut sums = BTreeMap::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let (hash, path) = line
            .split_once("  ")
            .with_context(|| format!("invalid SHA256SUMS.txt line {}", index + 1))?;
        if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            bail!("invalid SHA-256 on line {}", index + 1);
        }
        if sums
            .insert(path.to_owned(), hash.to_ascii_lowercase())
            .is_some()
        {
            bail!("duplicate SHA256SUMS.txt path {path}");
        }
    }
    Ok(sums)
}

fn verify_archive(path: &Path) -> Result<ReleaseManifest> {
    let file = File::open(path).with_context(|| format!("open archive {}", path.display()))?;
    let mut archive = ZipArchive::new(file).context("open ZIP archive")?;
    if archive.is_empty() {
        bail!("release archive is empty");
    }
    let mut names = BTreeSet::new();
    let mut root_name: Option<String> = None;
    for index in 0..archive.len() {
        let entry = archive.by_index(index)?;
        if entry.is_dir() {
            continue;
        }
        let name = entry.name().to_owned();
        let (root, relative) = name
            .split_once('/')
            .context("every release file must be under one package root")?;
        if relative.is_empty() || relative.contains("../") || relative.starts_with('/') {
            bail!("unsafe release path {name:?}");
        }
        match &root_name {
            Some(expected) if expected != root => {
                bail!("release archive contains multiple roots: {expected:?} and {root:?}")
            }
            None => root_name = Some(root.to_owned()),
            _ => {}
        }
        if !names.insert(relative.to_owned()) {
            bail!("duplicate release path {relative:?}");
        }
    }
    let root_name = root_name.context("release archive contains no files")?;
    let manifest_bytes = read_zip_entry(&mut archive, &format!("{root_name}/{MANIFEST_NAME}"))?;
    let manifest: ReleaseManifest =
        serde_json::from_slice(&manifest_bytes).context("release-manifest.json is invalid")?;
    if manifest.schema_version != 1
        || manifest.package != "runwatch"
        || manifest.profile != "release"
    {
        bail!("unsupported release manifest identity/schema");
    }
    let expected_root = format!("runwatch-v{}-{}", manifest.version, manifest.platform);
    if root_name != expected_root {
        bail!("archive root {root_name:?} does not match manifest {expected_root:?}");
    }
    if !manifest.layout.sibling_binaries_required {
        bail!("release manifest does not require sibling binaries");
    }
    let file_paths = manifest
        .files
        .iter()
        .map(|entry| entry.path.as_str())
        .collect::<BTreeSet<_>>();
    for required in [
        manifest.layout.runwatch.as_str(),
        manifest.layout.runwatch_mcp.as_str(),
        manifest.layout.runwatch_gui.as_str(),
        "README.md",
        "docs/INSTALL.md",
    ] {
        if !file_paths.contains(required) {
            bail!("release manifest is missing required payload {required}");
        }
    }
    for entry in &manifest.files {
        let bytes = read_zip_entry(&mut archive, &format!("{root_name}/{}", entry.path))?;
        if bytes.len() as u64 != entry.bytes {
            bail!("size mismatch for {}", entry.path);
        }
        if sha256_bytes(&bytes) != entry.sha256 {
            bail!("SHA-256 mismatch for {}", entry.path);
        }
    }
    let sums_bytes = read_zip_entry(&mut archive, &format!("{root_name}/{SUMS_NAME}"))?;
    let sums = parse_sums(&sums_bytes)?;
    let mut expected_sum_paths = manifest
        .files
        .iter()
        .map(|entry| entry.path.clone())
        .collect::<BTreeSet<_>>();
    expected_sum_paths.insert(MANIFEST_NAME.into());
    if sums.keys().cloned().collect::<BTreeSet<_>>() != expected_sum_paths {
        bail!("SHA256SUMS.txt does not cover exactly the payload + manifest");
    }
    for (relative, expected_hash) in &sums {
        let bytes = read_zip_entry(&mut archive, &format!("{root_name}/{relative}"))?;
        if sha256_bytes(&bytes) != *expected_hash {
            bail!("SHA256SUMS.txt mismatch for {relative}");
        }
    }
    let mut expected_archive_paths = expected_sum_paths;
    expected_archive_paths.insert(SUMS_NAME.into());
    if names != expected_archive_paths {
        bail!("archive contains missing or unexpected files");
    }
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_payload(root: &Path) -> Vec<SourceEntry> {
        let mut entries = Vec::new();
        for (path, contents, mode) in [
            (native_name("runwatch"), b"runwatch\n".as_slice(), 0o755),
            (native_name("runwatch-mcp"), b"mcp\n".as_slice(), 0o755),
            (native_name("runwatch-gui"), b"gui\n".as_slice(), 0o755),
            ("README.md".into(), b"readme\n".as_slice(), 0o644),
            ("docs/INSTALL.md".into(), b"install\n".as_slice(), 0o644),
        ] {
            let source = root.join(path.replace('/', "_"));
            fs::write(&source, contents).unwrap();
            entries.push(source_entry(path, source, mode).unwrap());
        }
        entries.sort_by(|left, right| left.path.cmp(&right.path));
        entries
    }

    #[test]
    fn archive_is_deterministic_and_self_verifying() {
        let temp = tempfile::tempdir().unwrap();
        let payload = fixture_payload(temp.path());
        let manifest = render_manifest(&payload).unwrap();
        let sums = render_sums(&payload, &manifest);
        let entries = archive_entries(&payload, manifest, sums);
        let root_name = package_name();
        let one = temp.path().join("one.zip");
        let two = temp.path().join("two.zip");
        write_archive(&one, &root_name, &entries).unwrap();
        write_archive(&two, &root_name, &entries).unwrap();
        assert_eq!(sha256_file(&one).unwrap(), sha256_file(&two).unwrap());
        verify_archive(&one).unwrap();
    }

    #[test]
    fn hashing_large_payload_does_not_require_a_large_stack_buffer() {
        let bytes = vec![0x5a_u8; 4 * 1024 * 1024];
        let hash = sha256_reader(std::io::Cursor::new(&bytes)).unwrap();
        assert_eq!(hash, sha256_bytes(&bytes));
    }

    #[test]
    fn existing_archive_is_never_overwritten() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("existing.zip");
        fs::write(&path, b"keep-me").unwrap();
        let error = write_archive(&path, "runwatch-test", &[]).unwrap_err();
        assert!(error.to_string().contains("refusing to overwrite"));
        assert_eq!(fs::read(&path).unwrap(), b"keep-me");
    }

    #[test]
    fn sums_parser_rejects_duplicates() {
        let row = format!("{}  a\n{}  a\n", "0".repeat(64), "1".repeat(64));
        assert!(parse_sums(row.as_bytes()).is_err());
    }
}
