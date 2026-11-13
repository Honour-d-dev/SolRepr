//! Watcher events: which paths a batch names, and which can move the root set.

use camino::{Utf8Path, Utf8PathBuf};
use lsp_types::{FileChangeType, FileEvent, Url};

use crate::workspace::watch::{self, Watched};

fn event(path: &str) -> FileEvent {
    FileEvent {
        uri: Url::from_file_path(path).expect("file url"),
        typ: FileChangeType::CHANGED,
    }
}

/// A burst can name one path twice — two watchers over the same tree, or a
/// create followed by a change — and the server should act on it once.
#[test]
fn a_batch_is_deduplicated_by_path() {
    let events = vec![event("/w/a.sol"), event("/w/a.sol"), event("/w/b.sol")];

    assert_eq!(
        watch::changed_paths(&events),
        vec![Utf8PathBuf::from("/w/a.sol"), Utf8PathBuf::from("/w/b.sol")],
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
