//! Algorithms over content-addressed trees, shared by server and client.

use std::collections::BTreeMap;

use crate::{Change, EntryKind, Error, FileState, ObjectId, RepoPath, Result, Tree, TreeEntry};

/// Somewhere trees can be read from: the server's object store, or a client's
/// cache that fetches missing objects from the server on demand.
pub trait TreeSource {
    fn get_tree(&self, id: &ObjectId) -> Result<Tree>;
}

pub trait TreeSink: TreeSource {
    fn put_tree(&self, tree: &Tree) -> Result<ObjectId>;
}

/// Resolves `path` under `root`. The root path resolves to the root tree.
pub fn lookup<S: TreeSource + ?Sized>(src: &S, root: ObjectId, path: &RepoPath) -> Result<Option<TreeEntry>> {
    let mut entry = TreeEntry { kind: EntryKind::Tree, id: root };
    for component in path.components() {
        if entry.kind != EntryKind::Tree {
            return Ok(None);
        }
        match src.get_tree(&entry.id)?.get(component) {
            Some(child) => entry = *child,
            None => return Ok(None),
        }
    }
    Ok(Some(entry))
}

/// Every file at or under `prefix`, sorted by path.
pub fn walk_files<S: TreeSource + ?Sized>(
    src: &S,
    root: ObjectId,
    prefix: &RepoPath,
) -> Result<Vec<(RepoPath, FileState)>> {
    let mut out = Vec::new();
    if let Some(entry) = lookup(src, root, prefix)? {
        walk_entry(src, prefix, entry, &mut out)?;
    }
    Ok(out)
}

fn walk_entry<S: TreeSource + ?Sized>(
    src: &S,
    path: &RepoPath,
    entry: TreeEntry,
    out: &mut Vec<(RepoPath, FileState)>,
) -> Result<()> {
    match entry.as_file() {
        Some(state) => out.push((path.clone(), state)),
        None => {
            for (name, child) in src.get_tree(&entry.id)?.iter() {
                walk_entry(src, &path.join(name)?, *child, out)?;
            }
        }
    }
    Ok(())
}

/// The changes that turn `from` into `to`, restricted to `prefix`.
///
/// Subtrees with equal ids are skipped without being read, so the cost is
/// proportional to what changed, not to the size of the repository.
pub fn diff_trees<S: TreeSource + ?Sized>(
    src: &S,
    from: ObjectId,
    to: ObjectId,
    prefix: &RepoPath,
) -> Result<Vec<Change>> {
    let mut out = Vec::new();
    if from != to {
        let a = lookup(src, from, prefix)?;
        let b = lookup(src, to, prefix)?;
        diff_entries(src, prefix, a, b, &mut out)?;
    }
    out.sort_by(|x, y| x.path.cmp(&y.path));
    Ok(out)
}

fn diff_entries<S: TreeSource + ?Sized>(
    src: &S,
    path: &RepoPath,
    a: Option<TreeEntry>,
    b: Option<TreeEntry>,
    out: &mut Vec<Change>,
) -> Result<()> {
    if a == b {
        return Ok(());
    }
    let a_tree = a.filter(|e| e.kind == EntryKind::Tree);
    let b_tree = b.filter(|e| e.kind == EntryKind::Tree);
    match (a_tree, b_tree) {
        (Some(a), Some(b)) => {
            let (ta, tb) = (src.get_tree(&a.id)?, src.get_tree(&b.id)?);
            let mut names: Vec<&str> = ta.iter().map(|(n, _)| n).chain(tb.iter().map(|(n, _)| n)).collect();
            names.sort_unstable();
            names.dedup();
            for name in names {
                diff_entries(src, &path.join(name)?, ta.get(name).copied(), tb.get(name).copied(), out)?;
            }
        }
        _ => {
            // At most one side is a directory. Delete everything on the old
            // side that is not overwritten, then add the new side.
            let mut removed = Vec::new();
            if let Some(a) = a {
                walk_entry(src, path, a, &mut removed)?;
            }
            let mut added = Vec::new();
            if let Some(b) = b {
                walk_entry(src, path, b, &mut added)?;
            }
            let added_paths: std::collections::HashSet<&RepoPath> = added.iter().map(|(p, _)| p).collect();
            for (p, _) in &removed {
                if !added_paths.contains(p) {
                    out.push(Change::delete(p.clone()));
                }
            }
            out.extend(added.into_iter().map(|(p, state)| Change::set(p, state)));
        }
    }
    Ok(())
}

/// Applies `changes` to the tree `root` and returns the new root.
///
/// Fails with [`Error::PathNotFound`] when deleting a file that does not
/// exist and [`Error::PathConflict`] when a change would treat a file as a
/// directory or vice versa. Directories left empty are removed.
pub fn apply_changes<S: TreeSink + ?Sized>(sink: &S, root: ObjectId, changes: &[Change]) -> Result<ObjectId> {
    let pending: Vec<Pending<'_>> =
        changes.iter().map(|change| Pending { components: change.path.components().collect(), change }).collect();
    if pending.iter().any(|p| p.components.is_empty()) {
        return Err(Error::InvalidChanges("cannot change the repository root".into()));
    }
    let refs: Vec<&Pending<'_>> = pending.iter().collect();
    match apply_dir(sink, Some(root), &refs, 0)? {
        Some(id) => Ok(id),
        None => sink.put_tree(&Tree::new()),
    }
}

struct Pending<'a> {
    components: Vec<&'a str>,
    change: &'a Change,
}

/// Applies changes to the directory `tree_id` (or an empty one) at `depth`.
/// Returns `None` if the directory ends up empty.
fn apply_dir<S: TreeSink + ?Sized>(
    sink: &S,
    tree_id: Option<ObjectId>,
    changes: &[&Pending<'_>],
    depth: usize,
) -> Result<Option<ObjectId>> {
    let mut tree = match tree_id {
        Some(id) => sink.get_tree(&id)?,
        None => Tree::new(),
    };

    // For each child name: the change to the child itself, and changes below it.
    let mut groups: BTreeMap<&str, (Option<&Change>, Vec<&Pending<'_>>)> = BTreeMap::new();
    for pending in changes {
        let group = groups.entry(pending.components[depth]).or_default();
        if pending.components.len() == depth + 1 {
            group.0 = Some(pending.change);
        } else {
            group.1.push(pending);
        }
    }

    for (name, (direct, deeper)) in groups {
        let existing = tree.get(name).copied();
        let is_dir = existing.is_some_and(|e| e.kind == EntryKind::Tree);
        let conflict = |path: &RepoPath, reason| Error::PathConflict { path: path.clone(), reason };

        if deeper.is_empty() {
            let change = direct.expect("group has at least one change");
            match (&change.new, existing) {
                (_, Some(_)) if is_dir => return Err(conflict(&change.path, "is a directory")),
                (None, None) => return Err(Error::PathNotFound(change.path.clone())),
                (None, Some(_)) => {
                    tree.remove(name);
                }
                (Some(state), _) => tree.insert(name, (*state).into()),
            }
            continue;
        }

        let deleting_file = direct.is_some_and(|c| c.new.is_none());
        let base = match existing {
            Some(e) if is_dir => Some(e.id),
            Some(_) if deleting_file => None,
            Some(_) => return Err(conflict(&deeper[0].change.path, "parent is a file")),
            None => None,
        };
        let subtree = apply_dir(sink, base, &deeper, depth + 1)?;

        match direct {
            None => match subtree {
                Some(id) => tree.insert(name, TreeEntry { kind: EntryKind::Tree, id }),
                None => {
                    tree.remove(name);
                }
            },
            // Replace file `name` with a directory.
            Some(Change { path, new: None }) => {
                if is_dir {
                    return Err(conflict(path, "is a directory"));
                }
                if existing.is_none() {
                    return Err(Error::PathNotFound(path.clone()));
                }
                match subtree {
                    Some(id) => tree.insert(name, TreeEntry { kind: EntryKind::Tree, id }),
                    None => {
                        tree.remove(name);
                    }
                }
            }
            // Replace directory `name` with a file; the directory must now be empty.
            Some(Change { path, new: Some(state) }) => {
                if subtree.is_some() {
                    return Err(conflict(path, "directory is not empty"));
                }
                tree.insert(name, (*state).into());
            }
        }
    }

    if tree.is_empty() {
        Ok(None)
    } else {
        sink.put_tree(&tree).map(Some)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashMap;

    use super::*;
    use crate::{empty_tree_id, FileKind};

    #[derive(Default)]
    struct MemTrees(RefCell<HashMap<ObjectId, Tree>>);

    impl TreeSource for MemTrees {
        fn get_tree(&self, id: &ObjectId) -> Result<Tree> {
            if *id == empty_tree_id() {
                return Ok(Tree::new());
            }
            self.0.borrow().get(id).cloned().ok_or(Error::ObjectNotFound(*id))
        }
    }

    impl TreeSink for MemTrees {
        fn put_tree(&self, tree: &Tree) -> Result<ObjectId> {
            let id = tree.id();
            self.0.borrow_mut().insert(id, tree.clone());
            Ok(id)
        }
    }

    fn path(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    fn file(content: &str) -> FileState {
        FileState { kind: FileKind::File, blob: ObjectId::for_blob(content.as_bytes()) }
    }

    fn set(p: &str, content: &str) -> Change {
        Change::set(path(p), file(content))
    }

    fn files(store: &MemTrees, root: ObjectId) -> Vec<(String, FileState)> {
        walk_files(store, root, &RepoPath::root())
            .unwrap()
            .into_iter()
            .map(|(p, s)| (p.as_str().to_string(), s))
            .collect()
    }

    #[test]
    fn apply_and_walk() {
        let store = MemTrees::default();
        let root = apply_changes(&store, empty_tree_id(), &[set("a/b/c.txt", "1"), set("a/d.txt", "2"), set("e", "3")])
            .unwrap();
        assert_eq!(
            files(&store, root),
            vec![("a/b/c.txt".into(), file("1")), ("a/d.txt".into(), file("2")), ("e".into(), file("3"))]
        );
        assert_eq!(lookup(&store, root, &path("a/d.txt")).unwrap(), Some(file("2").into()));
        assert_eq!(lookup(&store, root, &path("a/d.txt/x")).unwrap(), None);
    }

    #[test]
    fn apply_is_order_independent_and_canonical() {
        let store = MemTrees::default();
        let one = apply_changes(&store, empty_tree_id(), &[set("x/1", "a"), set("y", "b")]).unwrap();
        let two = apply_changes(&store, empty_tree_id(), &[set("y", "b")]).unwrap();
        let two = apply_changes(&store, two, &[set("x/1", "a")]).unwrap();
        assert_eq!(one, two);
    }

    #[test]
    fn deletes_prune_empty_directories() {
        let store = MemTrees::default();
        let root = apply_changes(&store, empty_tree_id(), &[set("a/b/c", "1")]).unwrap();
        let root = apply_changes(&store, root, &[Change::delete(path("a/b/c"))]).unwrap();
        assert_eq!(root, empty_tree_id());
    }

    #[test]
    fn rejects_bad_applications() {
        let store = MemTrees::default();
        let root = apply_changes(&store, empty_tree_id(), &[set("a/b", "1")]).unwrap();
        assert!(matches!(apply_changes(&store, root, &[Change::delete(path("nope"))]), Err(Error::PathNotFound(_))));
        assert!(matches!(apply_changes(&store, root, &[set("a", "x")]), Err(Error::PathConflict { .. })));
        assert!(matches!(apply_changes(&store, root, &[set("a/b/c", "x")]), Err(Error::PathConflict { .. })));
    }

    #[test]
    fn file_and_directory_swaps() {
        let store = MemTrees::default();
        let root = apply_changes(&store, empty_tree_id(), &[set("a", "file")]).unwrap();
        // File -> directory.
        let dir = apply_changes(&store, root, &[Change::delete(path("a")), set("a/inner", "x")]).unwrap();
        assert_eq!(files(&store, dir), vec![("a/inner".into(), file("x"))]);
        // Directory -> file.
        let back = apply_changes(&store, dir, &[Change::delete(path("a/inner")), set("a", "file")]).unwrap();
        assert_eq!(back, root);
        // Diffs between the two shapes apply cleanly in both directions.
        for (from, to) in [(root, dir), (dir, root)] {
            let changes = diff_trees(&store, from, to, &RepoPath::root()).unwrap();
            assert_eq!(apply_changes(&store, from, &changes).unwrap(), to);
        }
    }

    #[test]
    fn diff_round_trips() {
        let store = MemTrees::default();
        let a = apply_changes(
            &store,
            empty_tree_id(),
            &[set("src/main.rs", "1"), set("src/lib.rs", "2"), set("README", "3")],
        )
        .unwrap();
        let b = apply_changes(
            &store,
            a,
            &[set("src/main.rs", "1b"), Change::delete(path("README")), set("docs/x.md", "4")],
        )
        .unwrap();
        let changes = diff_trees(&store, a, b, &RepoPath::root()).unwrap();
        assert_eq!(changes.iter().map(|c| c.path.as_str()).collect::<Vec<_>>(), ["README", "docs/x.md", "src/main.rs"]);
        assert_eq!(apply_changes(&store, a, &changes).unwrap(), b);

        let scoped = diff_trees(&store, a, b, &path("src")).unwrap();
        assert_eq!(scoped, vec![set("src/main.rs", "1b")]);
    }
}
