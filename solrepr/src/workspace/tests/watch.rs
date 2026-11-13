//! Watcher events: which paths a batch names, and which can move the root set.

use camino::{Utf8Path, Utf8PathBuf};
use lsp_types::{FileChangeType, FileEvent, Url};

use crate::utilities::normalize_path;
use crate::workspace::watch::{self, Watched};

/// An absolute path on every platform: `/w/a.sol` is not one on Windows, so
/// `Url::from_file_path` would refuse it.
fn path(name: &str) -> Utf8PathBuf {
    let temp = Utf8PathBuf::from_path_buf(std::env::temp_dir()).expect("temp dir is UTF-8");
    normalize_path(&temp.join(name))
}

fn event(path: &Utf8Path) -> FileEvent {
    FileEvent {
        uri: Url::from_file_path(path).expect("file url"),
        typ: FileChangeType::CHANGED,
    }
}

/// A burst can name one path twice — two watchers over the same tree, or a
/// create followed by a change — and the server should act on it once.
#[test]
fn a_batch_is_deduplicated_by_path() {
    let a = path("solrepr_watch_a.sol");
    let b = path("solrepr_watch_b.sol");
    let events = vec![event(&a), event(&a), event(&b)];

    assert_eq!(
        watch::changed_paths(&events),
        vec![a, b],
        "the first mention of each path survives, in order"
    );
}

/// A path the server cannot read is dropped rather than treated as a change.
#[test]
fn a_non_file_uri_is_dropped() {
    let events = vec![FileEvent {
        uri: Url::parse("untitled:Untitled-1").expect("uri"),
        typ: FileChangeType::CHANGED,
    }];

    assert!(watch::changed_paths(&events).is_empty());
}

#[test]
fn only_manifests_and_solidity_sources_are_watched() {
    assert_eq!(
        watch::classify(Utf8Path::new("/w/foundry.toml")),
        Some(Watched::Manifest)
    );
    assert_eq!(
        watch::classify(Utf8Path::new("/w/hardhat.config.mts")),
        Some(Watched::Manifest)
    );
    assert_eq!(
        watch::classify(Utf8Path::new("/w/src/A.sol")),
        Some(Watched::Source)
    );
    assert_eq!(watch::classify(Utf8Path::new("/w/README.md")), None);
    assert_eq!(watch::classify(Utf8Path::new("/w/src/A.txt")), None);
}
