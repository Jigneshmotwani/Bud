use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{Error, ObjectId};

const TREE_MAGIC: &[u8; 4] = b"BWT1";

/// The kind of a file stored in a blob.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileKind {
    File,
    Executable,
    /// The blob holds the link target.
    Symlink,
}

/// The kind of a tree entry: a file or a subdirectory.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    File,
    Executable,
    Symlink,
    Tree,
}

impl EntryKind {
    fn code(self) -> u8 {
        match self {
            EntryKind::File => 1,
            EntryKind::Executable => 2,
            EntryKind::Symlink => 3,
            EntryKind::Tree => 4,
        }
    }

    fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            1 => EntryKind::File,
            2 => EntryKind::Executable,
            3 => EntryKind::Symlink,
            4 => EntryKind::Tree,
            _ => return None,
        })
    }

    pub fn file_kind(self) -> Option<FileKind> {
        match self {
            EntryKind::File => Some(FileKind::File),
            EntryKind::Executable => Some(FileKind::Executable),
            EntryKind::Symlink => Some(FileKind::Symlink),
            EntryKind::Tree => None,
        }
    }
}

impl From<FileKind> for EntryKind {
    fn from(kind: FileKind) -> Self {
        match kind {
            FileKind::File => EntryKind::File,
            FileKind::Executable => EntryKind::Executable,
            FileKind::Symlink => EntryKind::Symlink,
        }
    }
}

/// The content of a file at some revision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FileState {
    pub kind: FileKind,
    pub blob: ObjectId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TreeEntry {
    pub kind: EntryKind,
    pub id: ObjectId,
}

impl TreeEntry {
    pub fn as_file(&self) -> Option<FileState> {
        self.kind.file_kind().map(|kind| FileState { kind, blob: self.id })
    }
}

impl From<FileState> for TreeEntry {
    fn from(state: FileState) -> Self {
        TreeEntry { kind: state.kind.into(), id: state.blob }
    }
}

/// One directory level. Entries are kept sorted by name so that the encoding,
/// and therefore the id, is canonical.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tree {
    entries: BTreeMap<String, TreeEntry>,
}

impl Tree {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, name: &str) -> Option<&TreeEntry> {
        self.entries.get(name)
    }

    pub fn insert(&mut self, name: impl Into<String>, entry: TreeEntry) {
        self.entries.insert(name.into(), entry);
    }

    pub fn remove(&mut self, name: &str) -> Option<TreeEntry> {
        self.entries.remove(name)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &TreeEntry)> {
        self.entries.iter().map(|(name, entry)| (name.as_str(), entry))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Canonical binary encoding:
    /// `"BWT1" count:u32le { kind:u8 name_len:u32le name id[32] }*`.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + self.entries.len() * 48);
        out.extend_from_slice(TREE_MAGIC);
        out.extend_from_slice(&(self.entries.len() as u32).to_le_bytes());
        for (name, entry) in &self.entries {
            out.push(entry.kind.code());
            out.extend_from_slice(&(name.len() as u32).to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(entry.id.as_bytes());
        }
        out
    }

    /// Decodes a tree, rejecting any non-canonical encoding so that
    /// `decode(bytes)?.id()` always equals the id the bytes were stored under.
    pub fn decode(bytes: &[u8]) -> Result<Tree, Error> {
        let corrupt = |msg: &str| Error::Corrupt(format!("tree: {msg}"));
        let mut reader = Reader(bytes);
        if reader.take(4).ok_or_else(|| corrupt("truncated"))? != TREE_MAGIC {
            return Err(corrupt("bad magic"));
        }
        let count = reader.u32().ok_or_else(|| corrupt("truncated"))?;
        let mut tree = Tree::new();
        let mut previous: Option<&str> = None;
        for _ in 0..count {
            let kind = reader.take(1).ok_or_else(|| corrupt("truncated"))?[0];
            let kind = EntryKind::from_code(kind).ok_or_else(|| corrupt("unknown entry kind"))?;
            let len = reader.u32().ok_or_else(|| corrupt("truncated"))? as usize;
            let name = reader.take(len).ok_or_else(|| corrupt("truncated"))?;
            let name = std::str::from_utf8(name).map_err(|_| corrupt("name is not UTF-8"))?;
            if name.is_empty() || name.contains(['/', '\0']) || name == "." || name == ".." {
                return Err(corrupt("invalid entry name"));
            }
            if previous.is_some_and(|prev| prev >= name) {
                return Err(corrupt("entries not sorted"));
            }
            previous = Some(name);
            let id = reader.take(ObjectId::LEN).ok_or_else(|| corrupt("truncated"))?;
            let id = ObjectId::from_bytes(id.try_into().expect("length checked"));
            tree.insert(name, TreeEntry { kind, id });
        }
        if !reader.0.is_empty() {
            return Err(corrupt("trailing bytes"));
        }
        Ok(tree)
    }

    pub fn id(&self) -> ObjectId {
        ObjectId::for_tree(&self.encode())
    }
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (head, tail) = self.0.split_at(n);
        self.0 = tail;
        Some(head)
    }

    fn u32(&mut self) -> Option<u32> {
        self.take(4).map(|b| u32::from_le_bytes(b.try_into().unwrap()))
    }
}

/// The id of the empty tree: the root of a repository with no files.
pub fn empty_tree_id() -> ObjectId {
    Tree::new().id()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Tree {
        let mut tree = Tree::new();
        let blob = ObjectId::for_blob(b"x");
        tree.insert("b.txt", TreeEntry { kind: EntryKind::File, id: blob });
        tree.insert("a", TreeEntry { kind: EntryKind::Tree, id: empty_tree_id() });
        tree.insert("run.sh", TreeEntry { kind: EntryKind::Executable, id: blob });
        tree
    }

    #[test]
    fn round_trip() {
        let tree = sample();
        let decoded = Tree::decode(&tree.encode()).unwrap();
        assert_eq!(decoded, tree);
        assert_eq!(decoded.id(), tree.id());
    }

    #[test]
    fn rejects_unsorted_and_trailing() {
        let mut bytes = sample().encode();
        bytes.push(0);
        assert!(Tree::decode(&bytes).is_err());

        // Hand-build an encoding with entries out of order.
        let id = ObjectId::for_blob(b"x");
        let mut bad = TREE_MAGIC.to_vec();
        bad.extend_from_slice(&2u32.to_le_bytes());
        for name in ["b", "a"] {
            bad.push(1);
            bad.extend_from_slice(&1u32.to_le_bytes());
            bad.extend_from_slice(name.as_bytes());
            bad.extend_from_slice(id.as_bytes());
        }
        assert!(Tree::decode(&bad).is_err());
    }
}
