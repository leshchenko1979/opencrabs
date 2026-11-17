//! Release discovery: the fork's rolling `fork-latest` release probe, its
//! status-aware diagnostics, platform-asset presence and the two version
//! comparators (upstream semver, and the fork's deploy cycle).

use crate::utils::install::{InstallMethod, platform_suffix};

/// The release this build probes for updates.
///
/// Fork-standing: `leshchenko1979/opencrabs` publishes ONE rolling release per
/// deploy cycle under the constant, non-semver tag `fork-latest`. Its **name**
/// carries the real version (`0.5.4+fork.430`) and its assets are named
/// `opencrabs-fork-<suffix>.<ext>` — no tag in the filename.
///
/// It used to probe `repos/adolfousier/opencrabs/releases/latest`: upstream,
/// which has never published a release at all, so the probe 404'd on every
/// call, `check_for_update()` returned `None` and self-update was silently
/// dead on every fork binary.
pub(super) const GITHUB_API: &str =
    "https://api.github.com/repos/leshchenko1979/opencrabs/releases/tags/fork-latest";

/// Build an honest, status-aware error string for a non-success
/// response from the release probe. Replaces the prior hardcoded
/// "rate limited or unavailable" suffix that lied about every
/// non-2xx — a real 404 (no published release) and a 403 (rate
/// limit) looked identical to the user, sending us down wrong
/// debug paths.
///
/// `body_excerpt` should be the first ~300 chars of the response
/// body so the message can quote the API's own explanation when
/// it returns one (GitHub error envelopes carry a useful `message`
/// field, e.g. "API rate limit exceeded for ...").
pub(crate) fn diagnose_releases_latest_status(
    status: reqwest::StatusCode,
    body_excerpt: &str,
    ratelimit_remaining: Option<&str>,
    ratelimit_reset: Option<&str>,
) -> String {
    let code = status.as_u16();
    let body_tail = if body_excerpt.trim().is_empty() {
        String::new()
    } else {
        format!(" — API said: {}", body_excerpt.trim())
    };
    let ratelimit_tail = match (ratelimit_remaining, ratelimit_reset) {
        (Some(r), Some(reset)) => {
            format!(" [x-ratelimit-remaining={r}, x-ratelimit-reset={reset}]")
        }
        (Some(r), None) => format!(" [x-ratelimit-remaining={r}]"),
        _ => String::new(),
    };
    match code {
        404 => format!(
            "GitHub returned 404 for the release probe — no published \
             (non-draft, non-prerelease) release exists at that tag for \
             this repo at this moment, or there's a brief \
             publish-propagation lag. Try again in a minute.{body_tail}{ratelimit_tail}"
        ),
        403 | 429 => format!(
            "GitHub rate limit hit ({code}) — unauthenticated requests \
             are capped at 60/hr per IP. Wait an hour, or set GITHUB_TOKEN \
             in your env to raise the cap to 5000/hr if you share this \
             IP.{body_tail}{ratelimit_tail}"
        ),
        500..=599 => format!(
            "GitHub API returned {code} — server-side issue, retry in a \
             few minutes.{body_tail}"
        ),
        _ => format!("GitHub API returned {status}.{body_tail}{ratelimit_tail}"),
    }
}

/// Check GitHub for a newer release. Returns `Some(latest_version)` if an
/// update is available **and** a binary asset exists for this platform,
/// `None` if already on latest, no asset ready, or on error.
pub async fn check_for_update() -> Option<String> {
    let current_version = crate::VERSION;
    let client = reqwest::Client::new();
    let resp = match client
        .get(GITHUB_API)
        .header("User-Agent", format!("opencrabs/{}", current_version))
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                target: "evolve",
                url = GITHUB_API,
                error = %e,
                "background update check failed to reach GitHub"
            );
            return None;
        }
    };
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        let body_excerpt: String = body.chars().take(300).collect();
        tracing::warn!(
            target: "evolve",
            url = GITHUB_API,
            %status,
            body_excerpt,
            "background update check: release probe returned non-2xx"
        );
        return None;
    }
    let release: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(
                target: "evolve",
                url = GITHUB_API,
                error = %e,
                "background update check: failed to parse release JSON"
            );
            return None;
        }
    };

    let (latest_tag, latest_version) = match release_identity(&release) {
        Some(pair) => pair,
        None => {
            tracing::warn!(
                target: "evolve",
                "background update check: release payload carries neither a \
                 usable name nor a tag_name"
            );
            return None;
        }
    };

    if !is_newer_release(&latest_version, current_version) {
        return None;
    }

    // If running from source, check if Cargo.toml already has the latest version
    if let Some(source_version) = source_cargo_version()
        && source_version == latest_version
    {
        return None;
    }

    // For pre-built binary installs, only report "available" if the platform
    // asset actually exists in the release (release may still be building).
    if matches!(InstallMethod::detect(), InstallMethod::PrebuiltBinary)
        && !has_platform_asset(&release, &latest_tag)
    {
        tracing::debug!(
            "Release {} exists but no asset for this platform yet",
            latest_tag
        );
        return None;
    }

    Some(latest_version)
}

/// The `(tag, version)` a release payload advertises.
///
/// The fork's rolling release has a constant, non-semver tag (`fork-latest`)
/// and carries the real version in the release **name** (`0.5.4+fork.430`), so
/// the name wins; `tag_name` is the fallback (upstream tags are `v0.5.4`). A
/// leading `v` is stripped either way — a no-op for fork versions.
///
/// Returns `None` when the payload carries neither a usable name nor a tag, so
/// callers can bail out instead of comparing the empty string.
pub(crate) fn release_identity(release: &serde_json::Value) -> Option<(String, String)> {
    let tag = release["tag_name"].as_str().unwrap_or("").trim();
    let raw = release["name"]
        .as_str()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .or_else(|| (!tag.is_empty()).then_some(tag))?;
    let version = raw.strip_prefix('v').unwrap_or(raw);
    if version.is_empty() {
        return None;
    }
    Some((tag.to_string(), version.to_string()))
}

/// Every release-asset filename that satisfies "the binary for this platform",
/// in preference order. ONE definition, shared by the update check
/// (`has_platform_asset`) and the download (`evolve_via_binary_download`): a
/// check that passes while the download 404s is exactly the failure this
/// exists to prevent, so both call sites must agree by construction.
///
/// 1. `opencrabs-fork-<suffix>.<ext>` — the fork's rolling `fork-latest`
///    release; its assets carry no tag in the filename.
/// 2. `opencrabs-<tag>-<suffix>.<ext>` — upstream's versioned convention, kept
///    so an upstream release still resolves if `GITHUB_API` is ever repointed
///    back at `releases/latest`.
/// 3. `opencrabs-<suffix>.<ext>` — the legacy unversioned convention.
pub(crate) fn platform_asset_candidates(tag: &str, suffix: &str) -> Vec<String> {
    let ext = if std::env::consts::OS == "windows" {
        "zip"
    } else {
        "tar.gz"
    };
    vec![
        format!("opencrabs-fork-{suffix}.{ext}"),
        format!("opencrabs-{tag}-{suffix}.{ext}"),
        format!("opencrabs-{suffix}.{ext}"),
    ]
}

/// Check whether the release JSON contains a downloadable asset for the
/// current platform.
pub(crate) fn has_platform_asset(release: &serde_json::Value, tag: &str) -> bool {
    let Some(suffix) = platform_suffix() else {
        return false;
    };
    let candidates = platform_asset_candidates(tag, suffix);
    release["assets"]
        .as_array()
        .map(|arr| {
            arr.iter().any(|a| {
                a["name"]
                    .as_str()
                    .is_some_and(|name| candidates.iter().any(|c| c == name))
            })
        })
        .unwrap_or(false)
}

/// Split a fork version of the form `<upstream-ver>+fork.<cycle>`.
///
/// Returns `None` for anything else — a plain semver, upstream's `v0.5.4`, the
/// `fork-latest` tag itself — so callers can fall back to semver comparison.
pub(crate) fn parse_fork_version(v: &str) -> Option<(&str, u64)> {
    let (base, cycle) = v.split_once("+fork.")?;
    if base.is_empty() {
        return None;
    }
    Some((base, cycle.trim().parse().ok()?))
}

/// The upstream part of a version string, dropping any `+fork.<cycle>` suffix.
fn upstream_base(v: &str) -> &str {
    v.split_once("+fork.").map(|(base, _)| base).unwrap_or(v)
}

/// Fork-aware "is `latest` newer than `current`", used by the update checks.
///
/// Upstream semver and the fork's deploy cycle are different axes: the fork
/// republishes with an unchanged upstream version (`0.5.4+fork.430` ->
/// `0.5.4+fork.431`), which a semver comparison can never see. When BOTH sides
/// carry `+fork.<cycle>` the cycle is compared numerically; otherwise this
/// falls back to `is_newer`'s semver comparison, unchanged.
///
/// Equal on both sides => not newer (never re-download in a loop).
pub(crate) fn is_newer_release(latest: &str, current: &str) -> bool {
    match (parse_fork_version(latest), parse_fork_version(current)) {
        (Some((latest_base, latest_cycle)), Some((current_base, current_cycle))) => {
            // Upstream version first: a bump there is an update whatever the
            // cycle says. Same upstream version => the cycle IS the axis.
            if is_newer(latest_base, current_base) {
                true
            } else if is_newer(current_base, latest_base) {
                false
            } else {
                latest_cycle > current_cycle
            }
        }
        // Mixed (one side plain semver, or an upstream `v0.5.4`): compare the
        // upstream base only. Handing a `+fork.N` string to `is_newer` would
        // let it read the cycle as a fourth semver field (`0.5.4+fork.431` ->
        // `[0, 5, 431]`) and report a fork build as newer than a real upstream
        // bump.
        _ => is_newer(upstream_base(latest), upstream_base(current)),
    }
}

/// Compare semver strings: returns true if `latest` is strictly newer than `current`.
pub fn is_newer(latest: &str, current: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> { v.split('.').filter_map(|s| s.parse().ok()).collect() };
    let l = parse(latest);
    let c = parse(current);
    l > c
}

/// Try to read the version from the source Cargo.toml relative to the running
/// binary. Returns `None` if not running from a source build or file not found.
pub(super) fn source_cargo_version() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let target_dir = exe.parent()?;
    let repo_root = target_dir.parent()?.parent()?;
    let cargo_toml = repo_root.join("Cargo.toml");
    let content = std::fs::read_to_string(&cargo_toml).ok()?;
    let table: toml::Table = content.parse().ok()?;
    table
        .get("package")?
        .get("version")?
        .as_str()
        .map(String::from)
}
