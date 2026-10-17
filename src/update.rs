//! Auto-update, backed by `gpui-updater-core`.
//!
//! The parts that are easy to get wrong are already solved there: semantic
//! version comparison, picking the artifact for the running platform, checksum
//! and signature checks, and -- the one that actually matters -- a staged,
//! same-volume bundle swap on macOS that rolls back rather than leaving no app
//! behind. Writing that again here would only mean writing it worse.
//!
//! What is left is this repository's specifics: which repo, which asset naming
//! convention, and how to phrase the result for the UI.

use gpui_updater_core::{EngineConfig, GitHubSource, UpdateEngine};
use semver::Version;

const OWNER: &str = "tsukiyo20";
const REPO: &str = "displaymux";

/// The checksums file published alongside each release, as `shasum -a 256`
/// writes it. Requiring it means a corrupted or truncated download is refused
/// before anything is unpacked.
const SUMS_ASSET: &str = "SHA256SUMS";

/// A release worth installing.
#[derive(Debug, Clone)]
pub struct Available {
    pub version: Version,
    pub asset_name: String,
    /// The release page, for anyone who would rather install by hand.
    pub page: String,
}

pub fn current() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("package version is semver")
}

/// Releases are published per architecture, and the asset name carries it, so
/// an Apple Silicon build never downloads an Intel one. A universal build would
/// have to drop this filter.
fn arch_pattern() -> &'static str {
    match std::env::consts::ARCH {
        "aarch64" => "arm64",
        other => other,
    }
}

fn engine() -> UpdateEngine<GitHubSource> {
    let source = GitHubSource::new(OWNER, REPO)
        .with_checksums(SUMS_ASSET)
        .asset_contains(arch_pattern());
    UpdateEngine::new(source, EngineConfig::new(current()))
}

/// Is there a newer release?
///
/// `Ok(None)` covers both "up to date" and "the repository has no releases
/// yet", which are the same thing to a caller and must not be reported as a
/// failure.
pub fn check() -> Result<Option<Available>, String> {
    let release = match engine().check() {
        Ok(release) => release,
        // A repository with no releases answers 404; that is not an error.
        Err(e) if is_missing(&e) => None,
        Err(e) => return Err(e.to_string()),
    };
    Ok(release.map(|r| Available {
        page: format!(
            "https://github.com/{OWNER}/{REPO}/releases/tag/v{}",
            r.version
        ),
        version: r.version,
        asset_name: r.asset.name,
    }))
}

/// Download the newer release and put it in place.
///
/// On macOS the new build is started before this returns and the caller is
/// expected to quit. Windows cannot replace a running executable, so its
/// installer hands the swap to a helper that waits for this process to exit.
pub fn install(progress: impl FnMut(u64, Option<u64>) + 'static) -> Result<(), String> {
    engine()
        .update_now(progress)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Whether the source said "there is nothing here" rather than failing.
fn is_missing(error: &impl std::fmt::Display) -> bool {
    let text = error.to_string().to_ascii_lowercase();
    text.contains("404") || text.contains("not found")
}
