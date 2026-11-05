use la_arena::{Arena, Idx};
use rustc_hash::FxHashMap;

use crate::salsa::File;

type NodeId = Idx<Node>;

#[derive(Clone, Copy)]
struct Node {
    key: File,
    version: i32,
    previous: Option<NodeId>,
    next: Option<NodeId>,
}

/// The open documents, split into a bounded LRU set whose members get
/// diagnosed and the `evicted` overflow. Eviction moves a document's version
/// rather than dropping it: an evicted document is still open on the client,
/// so its edits must keep applying. Only `didClose` removes a document.
pub(crate) struct LiveDocs {
    map: FxHashMap<File, NodeId>,
    evicted: FxHashMap<File, i32>,
    nodes: Arena<Node>,
    dead_nodes: Vec<NodeId>,
    most_recent: Option<NodeId>,
    least_recent: Option<NodeId>,
    capacity: usize,
}

impl LiveDocs {
    pub(crate) fn new() -> Self {
        Self::with_capacity(64)
    }

    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self {
            map: FxHashMap::default(),
            evicted: FxHashMap::default(),
            nodes: Arena::with_capacity(capacity),
            dead_nodes: Vec::new(),
            most_recent: None,
            least_recent: None,
            capacity,
        }
    }

    /// The size of the diagnosed set, not counting evicted documents.
    pub(crate) fn len(&self) -> usize {
        self.map.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// `file` is in the diagnosed set.
    pub(crate) fn is_live(&self, file: &File) -> bool {
        self.map.contains_key(file)
    }

    /// `file` is open on the client, whether diagnosed or evicted.
    pub(crate) fn is_open(&self, file: &File) -> bool {
        self.map.contains_key(file) || self.evicted.contains_key(file)
    }

    #[allow(dead_code)]
    pub(crate) fn most_recent(&self) -> Option<File> {
        self.most_recent.map(|node_id| self.nodes[node_id].key)
    }

    #[allow(dead_code)]
    pub(crate) fn least_recent(&self) -> Option<File> {
        self.least_recent.map(|node_id| self.nodes[node_id].key)
    }

    pub(crate) fn iter(&self) -> LiveDocIter<'_> {
        LiveDocIter {
            nodes: &self.nodes,
            next: self.most_recent,
        }
    }

    /// Marks `file` as the most recently used document at `version`, returning
    /// the document evicted to make room (if any). Eviction only leaves the
    /// diagnosed set — the document stays open and keeps its version.
    pub(crate) fn touch(&mut self, file: File, version: i32) -> Option<File> {
        if self.promote(&file) {
            let node_id = self.map[&file];
            self.nodes[node_id].version = version;
            return None;
        }
        self.evicted.remove(&file);
        self.insert(file, version)
    }

    /// Removes `file` from whichever set holds it — only `didClose` should call
    /// this, since an open document is never untracked.
    pub(crate) fn close(&mut self, file: &File) -> bool {
        self.remove(file) || self.evicted.remove(file).is_some()
    }

    /// The version the client last reported for `file`, or `None` when it is
    /// not open.
    pub(crate) fn version(&self, file: &File) -> Option<i32> {
        self.map
            .get(file)
            .map(|node_id| self.nodes[*node_id].version)
            .or_else(|| self.evicted.get(file).copied())
    }

    fn insert(&mut self, file: File, version: i32) -> Option<File> {
        let evicted = (self.len() >= self.capacity)
            .then(|| self.pop_lru())
            .flatten();
        let node_id = self.alloc_node(file, version);
        self.attach_front(node_id);
        self.map.insert(file, node_id);
        evicted
    }

    /// Promotes a file to MRU if it exists, otherwise returns false.
    fn promote(&mut self, file: &File) -> bool {
        let Some(&node_id) = self.map.get(file) else {
            return false;
        };
        match self.most_recent {
            Some(most_recent_id) if most_recent_id == node_id => true,
            _ => {
                self.detach(node_id);
                self.attach_front(node_id);
                true
            }
        }
    }

    fn remove(&mut self, file: &File) -> bool {
        let Some(node_id) = self.map.remove(file) else {
            return false;
        };
        self.detach(node_id);
        self.dead_nodes.push(node_id);
        true
    }

    /// Evicts the least recently used member of the diagnosed set. The document
    /// stays open, so its version moves to `evicted` rather than dropping.
    fn pop_lru(&mut self) -> Option<File> {
        let node_id = self.least_recent?;
        let node = self.nodes[node_id];
        self.remove(&node.key);
        self.evicted.insert(node.key, node.version);
        Some(node.key)
    }

    fn alloc_node(&mut self, file: File, version: i32) -> NodeId {
        let node = Node {
            key: file,
            version,
            previous: None,
            next: None,
        };
        if let Some(node_id) = self.dead_nodes.pop() {
            self.nodes[node_id] = node;
            node_id
        } else {
            self.nodes.alloc(node)
        }
    }

    fn detach(&mut self, node_id: NodeId) {
        let node = self.nodes[node_id];
        match node.previous {
            Some(previous) => self.nodes[previous].next = node.next,
            None => self.most_recent = node.next,
        }
        match node.next {
            Some(next) => self.nodes[next].previous = node.previous,
            None => self.least_recent = node.previous,
        }
        self.nodes[node_id].previous = None;
        self.nodes[node_id].next = None;
    }

    fn attach_front(&mut self, node_id: NodeId) {
        self.nodes[node_id].previous = None;
        self.nodes[node_id].next = self.most_recent;
        if let Some(most_recent) = self.most_recent {
            self.nodes[most_recent].previous = Some(node_id);
        } else {
            self.least_recent = Some(node_id);
        }
        self.most_recent = Some(node_id);
    }
}

pub(crate) struct LiveDocIter<'a> {
    nodes: &'a Arena<Node>,
    next: Option<NodeId>,
}

impl Iterator for LiveDocIter<'_> {
    type Item = File;

    fn next(&mut self) -> Option<Self::Item> {
        let node_id = self.next?;
        let node = self.nodes[node_id];
        self.next = node.next;
        Some(node.key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use ropey::Rope;

    use crate::salsa::Db;

    #[test]
    fn maintains_mru_order_and_evicts_lru() {
        let db = Db::default();
        let file_a = File::builder(Rope::from_str(""), Utf8PathBuf::from("a.sol")).new(&db);
        let file_b = File::builder(Rope::from_str(""), Utf8PathBuf::from("b.sol")).new(&db);
        let file_c = File::builder(Rope::from_str(""), Utf8PathBuf::from("c.sol")).new(&db);
        let mut files = LiveDocs::with_capacity(2);

        assert_eq!(files.touch(file_a, 1), None);
        assert_eq!(files.touch(file_b, 1), None);
        assert_eq!(files.iter().collect::<Vec<_>>(), vec![file_b, file_a]);

        assert!(files.promote(&file_a));
        assert_eq!(files.iter().collect::<Vec<_>>(), vec![file_a, file_b]);
        assert_eq!(files.touch(file_c, 1), Some(file_b));
        assert_eq!(files.iter().collect::<Vec<_>>(), vec![file_c, file_a]);
    }

    #[test]
    fn reuses_removed_nodes() {
        let db = Db::default();
        let file_a = File::builder(Rope::from_str(""), Utf8PathBuf::from("a.sol")).new(&db);
        let file_b = File::builder(Rope::from_str(""), Utf8PathBuf::from("b.sol")).new(&db);
        let mut files = LiveDocs::new();

        files.touch(file_a, 1);
        assert!(files.close(&file_a));
        files.touch(file_b, 1);

        assert_eq!(files.len(), 1);
        assert_eq!(files.most_recent(), Some(file_b));
        assert_eq!(files.least_recent(), Some(file_b));
    }

    /// A document's version follows it: touching again replaces it, and
    /// eviction moves it to the overflow set rather than dropping it — an
    /// evicted document is still open.
    #[test]
    fn versions_track_the_document() {
        let db = Db::default();
        let file_a = File::builder(Rope::from_str(""), Utf8PathBuf::from("a.sol")).new(&db);
        let file_b = File::builder(Rope::from_str(""), Utf8PathBuf::from("b.sol")).new(&db);
        let mut files = LiveDocs::with_capacity(1);

        assert_eq!(files.version(&file_a), None);
        files.touch(file_a, 1);
        assert_eq!(files.version(&file_a), Some(1));
        files.touch(file_a, 4);
        assert_eq!(files.version(&file_a), Some(4));

        // Opening a second document evicts the first, but it keeps its
        // version and stays open — only `remove` untracks it.
        assert_eq!(files.touch(file_b, 1), Some(file_a));
        assert!(files.is_open(&file_a));
        assert!(!files.is_live(&file_a));
        assert_eq!(files.version(&file_a), Some(4));
        assert_eq!(files.version(&file_b), Some(1));

        assert!(files.close(&file_a));
        assert_eq!(files.version(&file_a), None);
        assert!(!files.is_open(&file_a));
    }

    /// Editing an evicted document re-tracks it: it rejoins the diagnosed set
    /// at the front and evicts whoever is now least recent.
    #[test]
    fn touching_an_evicted_document_retracks_it() {
        let db = Db::default();
        let file_a = File::builder(Rope::from_str(""), Utf8PathBuf::from("a.sol")).new(&db);
        let file_b = File::builder(Rope::from_str(""), Utf8PathBuf::from("b.sol")).new(&db);
        let mut files = LiveDocs::with_capacity(1);

        files.touch(file_a, 1);
        assert_eq!(files.touch(file_b, 2), Some(file_a));

        // A change arrives for the still-open evicted document; it rejoins the
        // live set and the other document takes its place in `evicted`.
        assert_eq!(files.touch(file_a, 3), Some(file_b));
        assert!(files.is_live(&file_a));
        assert_eq!(files.version(&file_a), Some(3));
        assert!(files.is_open(&file_b));
        assert!(!files.is_live(&file_b));
        assert_eq!(files.version(&file_b), Some(2));
    }
}
