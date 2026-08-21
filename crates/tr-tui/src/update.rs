//! Self-update from GitHub release artifacts.

use std::{
    env, fs,
    io::Read,
    sync::mpsc::{Receiver, Sender, channel},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};

const REPO: &str = "Kardzhilov/TerminalReader";

/// Version of this binary: the release tag baked in by CI, or the crate
/// version for local builds.
#[must_use]
pub fn current_version() -> &'static str {
    match option_env!("TR_RELEASE_VERSION") {
        Some(version) => version,
        None => env!("CARGO_PKG_VERSION"),
    }
}

#[derive(Debug, Clone)]
pub struct UpdateCheck {
    pub current: String,
    pub latest: String,
    pub available: bool,
}

fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    let mut parts = text.trim().trim_start_matches('v').split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    parts.next().is_none().then_some((major, minor, patch))
}

fn target_triple() -> Result<&'static str> {
    Ok(match (env::consts::OS, env::consts::ARCH) {
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("windows", "x86_64") => "x86_64-pc-windows-msvc",
        (os, arch) => bail!("no release artifacts for {os}/{arch}"),
    })
}

fn http_client() -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        // GitHub's API rejects requests without a User-Agent.
        .user_agent(concat!("terminalreader/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(60))
        .build()?)
}

/// Query the latest release and compare it with the running version.
pub fn check() -> Result<UpdateCheck> {
    #[derive(Deserialize)]
    struct Release {
        tag_name: String,
    }
    let release: Release = http_client()?
        .get(format!(
            "https://api.github.com/repos/{REPO}/releases/latest"
        ))
        .send()
        .context("could not reach github.com")?
        .error_for_status()
        .context("no published release found")?
        .json()?;
    let current = current_version().to_owned();
    let available = match (parse_version(&release.tag_name), parse_version(&current)) {
        (Some(latest), Some(current)) => latest > current,
        _ => false,
    };
    Ok(UpdateCheck {
        current,
        latest: release.tag_name,
        available,
    })
}

/// Download release `tag` for this platform, verify its published SHA-256
/// checksum, and replace the running binary.
pub fn apply(tag: &str) -> Result<()> {
    let target = target_triple()?;
    let extension = if cfg!(windows) { "zip" } else { "tar.gz" };
    let asset = format!("terminalreader-{tag}-{target}.{extension}");
    let url = format!("https://github.com/{REPO}/releases/download/{tag}/{asset}");
    let client = http_client()?;
    let archive = client
        .get(&url)
        .send()
        .context("could not reach github.com")?
        .error_for_status()
        .with_context(|| format!("release asset missing: {asset}"))?
        .bytes()?;
    verify_checksum(&client, tag, &asset, &archive)?;
    let binary = extract_binary(&archive, tag, target)?;
    replace_current_exe(&binary)
}

/// Refuse to install an archive whose SHA-256 does not match the sums file
/// published with the release.
fn verify_checksum(
    client: &reqwest::blocking::Client,
    tag: &str,
    asset: &str,
    archive: &[u8],
) -> Result<()> {
    let url = format!("https://github.com/{REPO}/releases/download/{tag}/SHA256SUMS.txt");
    let sums = client
        .get(&url)
        .send()
        .context("could not reach github.com")?
        .error_for_status()
        .context("release has no SHA256SUMS.txt; refusing unverified update")?
        .text()?;
    let expected = expected_sum(&sums, asset)
        .with_context(|| format!("SHA256SUMS.txt has no entry for {asset}"))?;
    let actual = hex::encode(Sha256::digest(archive));
    if !actual.eq_ignore_ascii_case(&expected) {
        bail!("checksum mismatch for {asset}: expected {expected}, got {actual}");
    }
    Ok(())
}

/// Find the checksum for `asset` in `sha256sum` output (`<hex>  <name>`,
/// with an optional `*` binary-mode marker).
fn expected_sum(sums: &str, asset: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let hash = parts.next()?;
        let name = parts.next()?;
        (name.trim_start_matches('*') == asset).then(|| hash.to_owned())
    })
}

fn extract_binary(archive: &[u8], tag: &str, target: &str) -> Result<Vec<u8>> {
    if cfg!(windows) {
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(archive))?;
        let name = format!("terminalreader-{tag}-{target}/terminalreader.exe");
        let mut file = zip
            .by_name(&name)
            .with_context(|| format!("archive did not contain {name}"))?;
        let mut binary = Vec::new();
        file.read_to_end(&mut binary)?;
        Ok(binary)
    } else {
        let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(archive));
        for entry in tar.entries()? {
            let mut entry = entry?;
            let is_binary = entry
                .path()?
                .file_name()
                .is_some_and(|name| name == "terminalreader");
            if is_binary {
                let mut binary = Vec::new();
                entry.read_to_end(&mut binary)?;
                return Ok(binary);
            }
        }
        bail!("archive did not contain the terminalreader binary");
    }
}

fn replace_current_exe(binary: &[u8]) -> Result<()> {
    let current = env::current_exe().context("could not locate the running executable")?;
    // Stage next to the target so the final rename stays on one filesystem.
    let staged = current.with_extension("new");
    fs::write(&staged, binary)
        .with_context(|| format!("could not write to {}", staged.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o755))?;
        fs::rename(&staged, &current)?;
    }
    #[cfg(not(unix))]
    {
        // Windows forbids replacing a running exe but allows renaming it away.
        let old = current.with_extension("old");
        let _ = fs::remove_file(&old);
        fs::rename(&current, &old)?;
        if let Err(error) = fs::rename(&staged, &current) {
            // Put the original binary back so the install stays usable.
            let _ = fs::rename(&old, &current);
            return Err(error.into());
        }
    }
    Ok(())
}

/// Remove the leftover binary from a previous Windows self-update.
pub fn clean_stale_backup() {
    if cfg!(windows) {
        if let Ok(current) = env::current_exe() {
            let _ = fs::remove_file(current.with_extension("old"));
        }
    }
}

#[derive(Debug)]
pub enum UpdateEvent {
    Checked(Result<UpdateCheck, String>),
    Applied(Result<String, String>),
}

/// Background update worker for the TUI.
#[derive(Debug)]
pub struct UpdateController {
    tx: Sender<UpdateEvent>,
    rx: Receiver<UpdateEvent>,
    pub busy: bool,
    /// Tag of a newer release found by the last check.
    pub available: Option<String>,
}

impl UpdateController {
    #[must_use]
    pub fn new() -> Self {
        let (tx, rx) = channel();
        Self {
            tx,
            rx,
            busy: false,
            available: None,
        }
    }

    pub fn check_in_background(&mut self) {
        if self.busy {
            return;
        }
        self.busy = true;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = check().map_err(|error| format!("{error:#}"));
            let _ = tx.send(UpdateEvent::Checked(result));
        });
    }

    pub fn apply_in_background(&mut self) {
        let Some(tag) = self.available.clone() else {
            return;
        };
        if self.busy {
            return;
        }
        self.busy = true;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = apply(&tag)
                .map(|()| tag)
                .map_err(|error| format!("{error:#}"));
            let _ = tx.send(UpdateEvent::Applied(result));
        });
    }

    pub fn poll(&mut self) -> Vec<UpdateEvent> {
        let mut events = Vec::new();
        while let Ok(event) = self.rx.try_recv() {
            self.busy = false;
            if let UpdateEvent::Checked(Ok(status)) = &event {
                self.available = status.available.then(|| status.latest.clone());
            }
            if let UpdateEvent::Applied(Ok(_)) = &event {
                self.available = None;
            }
            events.push(event);
        }
        events
    }
}

impl Default for UpdateController {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_parsing_handles_tags_and_plain_versions() {
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("10.0.1"), Some((10, 0, 1)));
        assert_eq!(parse_version("1.2"), None);
        assert_eq!(parse_version("1.2.3.4"), None);
        assert_eq!(parse_version("abc"), None);
    }

    #[test]
    fn newer_release_compares_greater() {
        let newer = parse_version("v1.10.0");
        let older = parse_version("v1.9.9");
        assert!(newer > older);
    }

    #[test]
    fn expected_sum_finds_asset_in_sha256sum_output() {
        let sums = "abc123  terminalreader-v1.0.0-x86_64-unknown-linux-gnu.tar.gz\n\
                    def456 *terminalreader-v1.0.0-x86_64-pc-windows-msvc.zip\n";
        assert_eq!(
            expected_sum(
                sums,
                "terminalreader-v1.0.0-x86_64-unknown-linux-gnu.tar.gz"
            )
            .as_deref(),
            Some("abc123")
        );
        assert_eq!(
            expected_sum(sums, "terminalreader-v1.0.0-x86_64-pc-windows-msvc.zip").as_deref(),
            Some("def456")
        );
        assert_eq!(expected_sum(sums, "other.zip"), None);
        assert_eq!(expected_sum("", "other.zip"), None);
    }
}
