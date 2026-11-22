//! Tests for the shared archive extractor (`crate::utils::archive`, #948).
//!
//! Three modules used to carry their own "pull one named member out of an
//! in-memory release archive" walk. The `.tar.gz` arms all matched the exact
//! basename; the `.zip` arms matched with `str::ends_with`, so a zip member
//! named `some-rtk` was returned when `rtk` was asked for. These tests pin the
//! corrected behaviour on both arms, in both directions: a suffixed name must
//! NOT match, a name under a leading directory MUST.
//!
//! Fixtures are built in memory from the `tar` / `flate2` / `zip` crates that
//! are already dependencies — no files on disk, no network.

use std::io::{ErrorKind, Write as _};

use flate2::Compression;
use flate2::write::GzEncoder;
use tar::{Builder, Header};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use crate::utils::archive::{extract_member_tar_gz, extract_member_zip};

/// Build an in-memory `.tar.gz` whose members are `(path, contents)`.
fn tar_gz_with(members: &[(&str, &[u8])]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    {
        let mut builder = Builder::new(&mut encoder);
        for (path, contents) in members {
            let mut header = Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(0o644);
            header.set_mtime(0);
            builder
                .append_data(&mut header, *path, *contents)
                .expect("append tar member");
        }
        builder.finish().expect("finish tar stream");
    }
    encoder.finish().expect("finish gzip stream")
}

/// Build an in-memory `.zip` whose members are `(path, contents)`.
fn zip_with(members: &[(&str, &[u8])]) -> Vec<u8> {
    let mut writer = ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    for (path, contents) in members {
        writer.start_file(*path, options).expect("start zip member");
        writer.write_all(contents).expect("write zip member");
    }
    writer.finish().expect("finish zip").into_inner()
}

// --- zip: the regression this commit fixes ---------------------------------

#[test]
fn zip_member_does_not_match_on_suffix() {
    // `x-rtk` ends with `rtk`. The old `ends_with` comparison returned it;
    // an exact basename comparison must not.
    let archive = zip_with(&[("x-rtk", b"wrong payload")]);

    let err = extract_member_zip(&archive, "rtk").expect_err("suffixed member must not match");
    assert_eq!(err.kind(), ErrorKind::NotFound);
}

#[test]
fn zip_member_matches_under_leading_directory() {
    // A leading directory inside the archive is fine — real release assets
    // ship `opencrabs-x.y.z-linux-x86_64/rtk`.
    let archive = zip_with(&[("opencrabs-0.5.4/rtk.exe", b"rtk payload")]);

    let data = extract_member_zip(&archive, "rtk.exe").expect("nested member must match");
    assert_eq!(data, b"rtk payload");
}

#[test]
fn zip_member_matches_exact_basename() {
    let archive = zip_with(&[("rtk.exe", b"rtk payload")]);

    let data = extract_member_zip(&archive, "rtk.exe").expect("exact member must match");
    assert_eq!(data, b"rtk payload");
}

#[test]
fn zip_member_missing_is_not_found() {
    let archive = zip_with(&[("some-other-binary", b"nope")]);

    let err = extract_member_zip(&archive, "rtk").expect_err("absent member must error");
    assert_eq!(err.kind(), ErrorKind::NotFound);
}

#[test]
fn zip_garbage_archive_errors_without_panicking() {
    // Not a zip at all: the reader must report an error, never panic.
    let err = extract_member_zip(b"this is not a zip archive", "rtk")
        .expect_err("garbage input must error");
    assert_ne!(err.kind(), ErrorKind::NotFound);
}

// --- tar.gz: behaviour preserved, pinned so it cannot drift ----------------

#[test]
fn tar_gz_member_does_not_match_on_suffix() {
    let archive = tar_gz_with(&[("x-rtk", b"wrong payload")]);

    let err = extract_member_tar_gz(&archive, "rtk").expect_err("suffixed member must not match");
    assert_eq!(err.kind(), ErrorKind::NotFound);
}

#[test]
fn tar_gz_member_matches_under_leading_directory() {
    let archive = tar_gz_with(&[("opencrabs-0.5.4-linux-x86_64/rtk", b"rtk payload")]);

    let data = extract_member_tar_gz(&archive, "rtk").expect("nested member must match");
    assert_eq!(data, b"rtk payload");
}

#[test]
fn tar_gz_member_matches_exact_basename() {
    let archive = tar_gz_with(&[("rtk", b"rtk payload")]);

    let data = extract_member_tar_gz(&archive, "rtk").expect("exact member must match");
    assert_eq!(data, b"rtk payload");
}

#[test]
fn tar_gz_member_missing_is_not_found() {
    let archive = tar_gz_with(&[("some-other-binary", b"nope")]);

    let err = extract_member_tar_gz(&archive, "rtk").expect_err("absent member must error");
    assert_eq!(err.kind(), ErrorKind::NotFound);
}

#[test]
fn tar_gz_garbage_archive_errors_without_panicking() {
    let err = extract_member_tar_gz(b"this is not a gzip stream", "rtk")
        .expect_err("garbage input must error");
    assert_ne!(err.kind(), ErrorKind::NotFound);
}

// --- both arms agree -------------------------------------------------------

#[test]
fn both_arms_pick_the_right_member_from_a_mixed_archive() {
    // The binary itself plus an RTK sitting beside it: asking for one must
    // never hand back the other.
    let members: &[(&str, &[u8])] = &[
        ("pkg/opencrabs", b"opencrabs payload"),
        ("pkg/rtk", b"rtk payload"),
        ("pkg/rtk.exe", b"rtk.exe payload"),
    ];

    let tar = tar_gz_with(members);
    assert_eq!(
        extract_member_tar_gz(&tar, "opencrabs").expect("tar opencrabs"),
        b"opencrabs payload"
    );
    assert_eq!(
        extract_member_tar_gz(&tar, "rtk").expect("tar rtk"),
        b"rtk payload"
    );
    assert_eq!(
        extract_member_tar_gz(&tar, "rtk.exe").expect("tar rtk.exe"),
        b"rtk.exe payload"
    );

    let zip = zip_with(members);
    assert_eq!(
        extract_member_zip(&zip, "opencrabs").expect("zip opencrabs"),
        b"opencrabs payload"
    );
    assert_eq!(
        extract_member_zip(&zip, "rtk").expect("zip rtk"),
        b"rtk payload"
    );
    assert_eq!(
        extract_member_zip(&zip, "rtk.exe").expect("zip rtk.exe"),
        b"rtk.exe payload"
    );
}
