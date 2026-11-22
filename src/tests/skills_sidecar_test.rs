//! The built-in skills sidecar (#946): extraction, install, and the clean
//! profile it has to serve.
//!
//! The eight built-in skill bodies are DATA beside the executable now. The
//! release archive carries `skills/<name>/SKILL.md`, `/evolve` writes that tree
//! out next to the binary it just installed, and the loader reads
//! `<exe_dir>/skills/`. Both halves are tested here rather than assumed: the
//! archive half (`extract_tree_*` + `install_tree`) and the loader half (a
//! profile with no skills of its own still sees all eight, badged
//! `SkillSource::Builtin`).
//!
//! The slugs are named literally on purpose. This is test code, compiled only
//! under `cfg(test)`, so it never reaches a shipped binary — and the point of
//! the issue is that the SHIPPED set is data, not that nothing anywhere may
//! spell the names.

use std::path::{Path, PathBuf};

use crate::brain::skills::{SkillSource, load_all_skills_from};
use crate::brain::tools::evolve::archive::{
    extract_tree_from_tar_gz, extract_tree_from_zip, install_tree, relative_under,
};

/// The eight built-ins the sidecar must deliver, by slug.
const BUILTIN_SLUGS: [&str; 8] = [
    "cost-estimate",
    "security-audit",
    "repo-audit",
    "browser-cdp",
    "a2a-gateway",
    "dynamic-tools",
    "github",
    "multi-agent",
];

/// Build a `.tar.gz` in memory, the way `release.yml` does on disk.
fn tar_gz(entries: &[(&str, &str)]) -> Vec<u8> {
    use flate2::Compression;
    use flate2::write::GzEncoder;

    let encoder = GzEncoder::new(Vec::new(), Compression::default());
    let mut builder = tar::Builder::new(encoder);
    for (path, body) in entries {
        let bytes = body.as_bytes();
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, *path, bytes)
            .expect("append tar member");
    }
    builder
        .into_inner()
        .expect("finish tar")
        .finish()
        .expect("finish gzip")
}

/// Build a `.zip` in memory, the way `release.yml` does for Windows.
fn zip_archive(entries: &[(&str, &str)]) -> Vec<u8> {
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (path, body) in entries {
        writer
            .start_file(*path, SimpleFileOptions::default())
            .expect("start zip member");
        writer.write_all(body.as_bytes()).expect("write zip member");
    }
    writer.finish().expect("finish zip").into_inner()
}

fn relative_paths(entries: &[(PathBuf, Vec<u8>)]) -> Vec<String> {
    let mut paths: Vec<String> = entries
        .iter()
        .map(|(p, _)| p.to_string_lossy().replace('\\', "/"))
        .collect();
    paths.sort();
    paths
}

#[test]
fn tar_extraction_keeps_only_the_skills_subtree() {
    let archive = tar_gz(&[
        ("opencrabs", "binary"),
        ("rtk", "rtk-binary"),
        ("build-flags.txt", "features"),
        ("skills/alpha/SKILL.md", "alpha"),
        ("skills/beta/SKILL.md", "beta"),
        ("docs/notes.md", "not a skill"),
    ]);

    let tree = extract_tree_from_tar_gz(&archive, "skills").expect("extract tree");
    assert_eq!(
        relative_paths(&tree),
        vec!["alpha/SKILL.md".to_string(), "beta/SKILL.md".to_string()],
        "only files below skills/ may come out of the archive"
    );
}

#[test]
fn zip_extraction_keeps_only_the_skills_subtree() {
    let archive = zip_archive(&[
        ("opencrabs.exe", "binary"),
        ("rtk.exe", "rtk-binary"),
        ("skills/alpha/SKILL.md", "alpha"),
        ("skills/beta/SKILL.md", "beta"),
    ]);

    let tree = extract_tree_from_zip(&archive, "skills").expect("extract tree");
    assert_eq!(
        relative_paths(&tree),
        vec!["alpha/SKILL.md".to_string(), "beta/SKILL.md".to_string()],
        "the zip path must mirror the tar.gz path"
    );
}

#[test]
fn an_archive_without_a_skills_tree_yields_nothing_and_is_not_an_error() {
    // Pre-#946 releases carry the binary and rtk only. That must update the
    // binary, not fail the evolve — an empty tree is the signal to leave the
    // installed skills dir alone.
    let archive = tar_gz(&[("opencrabs", "binary"), ("rtk", "rtk-binary")]);
    let tree =
        extract_tree_from_tar_gz(&archive, "skills").expect("missing subtree is not an error");
    assert!(tree.is_empty(), "got {:?}", relative_paths(&tree));

    let zip_bytes = zip_archive(&[("opencrabs.exe", "binary")]);
    let zip_tree =
        extract_tree_from_zip(&zip_bytes, "skills").expect("missing subtree is not an error");
    assert!(zip_tree.is_empty(), "got {:?}", relative_paths(&zip_tree));
}

#[test]
fn relative_under_refuses_paths_that_would_escape_the_destination() {
    // The archive is checksum-verified upstream, but a member path is still a
    // write primitive. Anything that is not a plain relative path below the
    // prefix is dropped, not sanitised.
    assert_eq!(
        relative_under(Path::new("skills/alpha/SKILL.md"), "skills"),
        Some(PathBuf::from("alpha/SKILL.md"))
    );
    assert_eq!(
        relative_under(Path::new("skills/../evil.md"), "skills"),
        None
    );
    assert_eq!(
        relative_under(Path::new("skills/../../etc/passwd"), "skills"),
        None
    );
    assert_eq!(relative_under(Path::new("/skills/abs.md"), "skills"), None);
    assert_eq!(relative_under(Path::new("skills"), "skills"), None);
    assert_eq!(relative_under(Path::new("other/SKILL.md"), "skills"), None);
}

#[test]
fn install_tree_replaces_the_existing_tree_atomically() {
    let dir = tempfile::tempdir().expect("tempdir");
    let dest = dir.path().join("skills");
    std::fs::create_dir_all(dest.join("legacy")).expect("seed legacy tree");
    std::fs::write(dest.join("legacy/SKILL.md"), "old").expect("seed legacy skill");

    let entries: Vec<(PathBuf, Vec<u8>)> = vec![
        (PathBuf::from("alpha/SKILL.md"), b"alpha".to_vec()),
        (PathBuf::from("beta/SKILL.md"), b"beta".to_vec()),
    ];
    let written = install_tree(&dest, &entries).expect("install tree");

    assert_eq!(written, 2);
    assert!(dest.join("alpha/SKILL.md").is_file());
    assert!(dest.join("beta/SKILL.md").is_file());
    assert!(
        !dest.join("legacy/SKILL.md").exists(),
        "the installed tree replaces the old one — a leftover skill would keep \
         shadowing what the release actually shipped"
    );

    let leftovers: Vec<String> = std::fs::read_dir(dir.path())
        .expect("read parent")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.starts_with(".skills.evolve"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "staging/retired dirs left behind: {leftovers:?}"
    );
}

#[test]
fn install_tree_creates_the_dir_when_none_existed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let dest = dir.path().join("skills");

    let entries: Vec<(PathBuf, Vec<u8>)> =
        vec![(PathBuf::from("alpha/SKILL.md"), b"alpha".to_vec())];
    assert_eq!(install_tree(&dest, &entries).expect("install tree"), 1);
    assert_eq!(
        std::fs::read_to_string(dest.join("alpha/SKILL.md")).expect("read back"),
        "alpha"
    );
}

#[test]
fn a_clean_profile_still_loads_all_eight_builtins_from_the_sidecar() {
    // The acceptance case from the issue: a profile with no `skills/` of its
    // own — no user overlay, no project overlays — must still see the full
    // built-in set, and it must be badged Builtin so the TUI keeps telling
    // them apart from skills the operator installed.
    let home = tempfile::tempdir().expect("tempdir");
    let profile_home = home.path().join(".opencrabs");
    std::fs::create_dir_all(&profile_home).expect("create profile home");

    crate::config::profile::with_home_override(profile_home, || {
        let skills = load_all_skills_from(Some(
            crate::tests::skills_test::shipped_skills_dir().as_path(),
        ));

        let mut names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        names.sort_unstable();
        let mut expected = BUILTIN_SLUGS.to_vec();
        expected.sort_unstable();
        assert_eq!(
            names, expected,
            "a clean profile must load exactly the eight sidecar skills"
        );
        for skill in &skills {
            assert_eq!(
                skill.source,
                SkillSource::Builtin,
                "{} came from the sidecar but is not badged Builtin",
                skill.name
            );
            assert!(
                !skill.body.is_empty(),
                "{} loaded with an empty body",
                skill.name
            );
        }
    });
}
