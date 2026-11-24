//! Looks, once per run, whether GitHub has a release newer than this build.
//!
//! One request, from a thread of its own, for the repository's latest release (drafts and
//! pre-releases are not that). A newer one shows as a notice in the status line that links to
//! the release page; nothing is downloaded or installed. A check that fails (offline, no
//! release yet, GitHub's rate limit) says nothing. Setting `VIBECAD_NO_UPDATE_CHECK` turns the
//! check off.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::Context;

const REPO: &str = "RawMechatronicsOrg/VibeCAD";

/// Major, minor, patch.
pub type Version = [u64; 3];

/// The newer release, once the check has found one.
#[derive(Clone, Default)]
pub struct Check(Arc<OnceLock<Version>>);

impl Check {
    /// Start the check; `repaint` is called if it finds a newer release. `Check::default()` is
    /// a check that never finds one.
    pub fn start(repaint: impl FnOnce() + Send + 'static) -> Check {
        let check = Check::default();
        // A build whose own version is not plain x.y.z is not compared with releases.
        let Some(current) = parse(env!("CARGO_PKG_VERSION")) else { return check };
        if std::env::var_os("VIBECAD_NO_UPDATE_CHECK").is_some() {
            return check;
        }
        let found = check.0.clone();
        std::thread::spawn(move || {
            if let Ok(latest) = latest_release(REPO)
                && latest > current
            {
                let _ = found.set(latest);
                repaint();
            }
        });
        check
    }

    pub fn newer(&self) -> Option<Version> {
        self.0.get().copied()
    }
}

/// The page of the latest release, with its downloads.
pub fn download_url() -> String {
    format!("https://github.com/{REPO}/releases/latest")
}

fn latest_release(repo: &str) -> anyhow::Result<Version> {
    let agent: ureq::Agent = ureq::Agent::config_builder().https_only(true).timeout_global(Some(Duration::from_secs(15))).build().into();
    let body = agent
        .get(format!("https://api.github.com/repos/{repo}/releases/latest"))
        // GitHub's API refuses requests without a user agent.
        .header("User-Agent", concat!("vibecad/", env!("CARGO_PKG_VERSION")))
        .header("Accept", "application/vnd.github+json")
        .call()?
        .body_mut()
        .read_to_string()?;
    let release: serde_json::Value = serde_json::from_str(&body)?;
    let tag = release["tag_name"].as_str().context("the release has no tag")?;
    parse(tag).with_context(|| format!("the release tag {tag:?} is not a version"))
}

/// `v1.2.3` or `1.2.3`. Anything else (a suffix, a missing part) is not a version.
fn parse(tag: &str) -> Option<Version> {
    let mut parts = tag.strip_prefix('v').unwrap_or(tag).split('.').map(|p| p.parse::<u64>().ok());
    let version = [parts.next()??, parts.next()??, parts.next()??];
    parts.next().is_none().then_some(version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_parse_as_versions_and_compare_by_number() {
        assert_eq!(parse("v0.1.0"), Some([0, 1, 0]));
        assert_eq!(parse("1.20.3"), Some([1, 20, 3]));
        assert!(parse("v0.10.0") > parse("v0.9.9"));
        assert!(parse("v1.0.0") > parse("v0.99.99"));
        for bad in ["", "v1.2", "v1.2.3.4", "v1.2.3-beta", "v1.2.x", "nightly"] {
            assert_eq!(parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn this_build_has_a_version_releases_compare_with() {
        assert!(parse(env!("CARGO_PKG_VERSION")).is_some());
    }

    /// Asks GitHub for the latest release of a repository that has releases.
    #[test]
    #[ignore = "needs the network"]
    fn the_latest_release_of_a_repository_is_read() {
        let v = latest_release("cli/cli").unwrap();
        assert!(v > [2, 0, 0], "{v:?}");
    }
}
