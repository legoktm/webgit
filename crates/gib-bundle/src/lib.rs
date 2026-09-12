//! Building a `git bundle`, without a server to run it on.
//!
//! A bundle is a clone in one file: a short text header naming the refs it
//! carries, then a packfile holding every object those refs reach. `git clone`,
//! `git fetch` and `git bundle verify` all read one, so a bundle built here is
//! the offline copy of a repository that a `.tar.gz` snapshot deliberately
//! isn't — it has the history in it, not just a tree.

#![deny(clippy::all)]

use futures::FutureExt;
use futures::future::LocalBoxFuture;
use futures::stream::{FuturesUnordered, StreamExt};
use gib_object::{Object, ObjectId, TreeEntryType};
use std::collections::{BTreeSet, VecDeque};

#[cfg(test)]
mod differential;
mod writer;

pub use writer::{BundleRef, BundleWriter, MAX_BUNDLE_BYTES};

/// How many object fetches the walk keeps in flight at once.
const MAX_IN_FLIGHT: usize = 48;

/// Where the walk reads objects from.
pub trait ObjectSource {
    /// Read one object, by id.
    fn object(&self, id: ObjectId) -> LocalBoxFuture<'_, anyhow::Result<Object>>;
}

/// Every object reachable from `tips`, in the order they should be packed.
pub async fn collect_objects<S: ObjectSource>(
    repo: &S,
    tips: &[ObjectId],
    on_progress: &dyn Fn(usize, usize),
) -> anyhow::Result<Vec<ObjectId>> {
    let mut walk = Walk {
        repo,
        seen: BTreeSet::new(),
        pending: VecDeque::new(),
        in_flight: FuturesUnordered::new(),
        commits: Vec::new(),
        tags: Vec::new(),
        trees: Vec::new(),
        blobs: Vec::new(),
        fetched: 0,
        requested: 0,
        report: on_progress,
    };
    for &tip in tips {
        // A tip is a ref, not a path: git names commits and tags with the empty
        // string here, and an empty name hashes to zero.
        walk.request(tip, 0);
    }
    walk.run().await?;
    Ok(walk.finish())
}

/// The history walk: what has been asked for, what is in flight, and what has
/// been found.
struct Walk<'a, S: ObjectSource> {
    repo: &'a S,
    /// Every id the walk has already dealt with, whether it was fetched or
    /// merely recorded. A repository's trees and blobs are shared across every
    /// revision that didn't change them, so this is what keeps the walk to the
    /// size of the object store rather than the size of the history.
    seen: BTreeSet<ObjectId>,
    /// Discovered but not yet requested, held back by [`MAX_IN_FLIGHT`], each
    /// with the hash of the path it was reached by (see [`name_hash`]).
    pending: VecDeque<(ObjectId, u32)>,
    in_flight: FuturesUnordered<LocalBoxFuture<'a, (u32, anyhow::Result<Object>)>>,
    commits: Vec<Found>,
    tags: Vec<Found>,
    trees: Vec<Found>,
    blobs: Vec<Found>,
    fetched: usize,
    requested: usize,
    report: &'a dyn Fn(usize, usize),
}

/// One object the walk found: its id, and the hash of the path it was reached
/// by, which is what the pack order groups on.
struct Found {
    id: ObjectId,
    name_hash: u32,
}

/// git's `pack_name_hash`, from `pack-objects.h`.
#[cfg(test)]
fn name_hash(name: &[u8]) -> u32 {
    extend_hash(0, name)
}

/// Carry a name hash on through `name`, as if it were appended to the path
/// hashed so far.
fn extend_hash(mut hash: u32, name: &[u8]) -> u32 {
    for &c in name {
        // git skips whitespace here, in the C locale's sense of the word.
        if matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r') {
            continue;
        }
        hash = (hash >> 2).wrapping_add((c as u32) << 24);
    }
    hash
}

impl<'a, S: ObjectSource> Walk<'a, S> {
    /// Fetch objects until nothing is left to fetch.
    ///
    /// One pool for every kind of object, unlike `gib-archive`'s split between
    /// blobs and subtrees: everything fetched here reveals more work, so there
    /// is no discovery to starve by letting them share a budget.
    async fn run(&mut self) -> anyhow::Result<()> {
        loop {
            self.issue();
            let Some(landed) = self.in_flight.next().await else {
                // Nothing in flight and nothing issued: the walk is done. (A
                // pool that empties while `pending` is non-empty can't happen —
                // `issue` above has just refilled it.)
                return Ok(());
            };
            self.fetched += 1;
            self.emit();
            let (name_hash, object) = landed;
            self.visit(object?, name_hash);
        }
    }

    /// Move discovered ids into the fetch pool, up to the budget.
    fn issue(&mut self) {
        let repo = self.repo;
        while self.in_flight.len() < MAX_IN_FLIGHT {
            let Some((id, name_hash)) = self.pending.pop_front() else {
                break;
            };
            self.in_flight.push(
                async move {
                    let object = repo
                        .object(id)
                        .await
                        .map_err(|e| anyhow::anyhow!("read object {id}: {e}"));
                    (name_hash, object)
                }
                .boxed_local(),
            );
        }
    }

    /// Record one object that has landed, and queue whatever it points at.
    ///
    /// `name_hash` is the hash of the path this object was reached by, which is
    /// what the pack order groups on; a directory's entries carry their
    /// parent's hash onward, so an object's hash is that of its whole path.
    fn visit(&mut self, object: Object, name_hash: u32) {
        let id = object.id();
        match object {
            Object::Commit(commit) => {
                self.commits.push(Found { id, name_hash });
                self.request(commit.tree(), 0);
                for &parent in commit.parents() {
                    self.request(parent, 0);
                }
            }
            Object::Tag(tag) => {
                self.tags.push(Found { id, name_hash });
                // A tag of a tag is legal and rare; either way what it points
                // at is fetched and classified like anything else, so nothing
                // here needs to know which it was.
                self.request(tag.target(), 0);
            }
            Object::Tree(tree) => {
                self.trees.push(Found { id, name_hash });
                let prefix = extend_hash(name_hash, b"/");
                for entry in tree.entries() {
                    let entry_hash = extend_hash(prefix, entry.name());
                    match entry.entry_type() {
                        TreeEntryType::Tree => self.request(entry.id(), entry_hash),
                        TreeEntryType::Commit => {}
                        TreeEntryType::File
                        | TreeEntryType::Executable
                        | TreeEntryType::Symlink => self.discover(entry.id(), entry_hash),
                    }
                }
            }
            // Only reachable through a tag that points straight at a blob,
            // which is rare but legal — an ordinary file is discovered from its
            // tree entry and never fetched by the walk at all.
            Object::Blob(_) => self.blobs.push(Found { id, name_hash }),
        }
    }

    /// Queue `id` to be fetched, unless the walk has met it before.
    fn request(&mut self, id: ObjectId, name_hash: u32) {
        if self.seen.insert(id) {
            self.pending.push_back((id, name_hash));
            self.requested += 1;
            self.emit();
        }
    }

    /// Record a blob the walk read out of a tree, which it does not need to
    /// fetch to pack.
    fn discover(&mut self, id: ObjectId, name_hash: u32) {
        if self.seen.insert(id) {
            self.blobs.push(Found { id, name_hash });
        }
    }

    fn emit(&self) {
        (self.report)(self.fetched, self.requested);
    }

    /// The packing order: see [`collect_objects`] for why it is this one.
    fn finish(self) -> Vec<ObjectId> {
        let Walk {
            mut commits,
            mut tags,
            mut trees,
            mut blobs,
            ..
        } = self;
        let mut out = Vec::with_capacity(commits.len() + tags.len() + trees.len() + blobs.len());
        // git's type order, largest type number first.
        for group in [&mut tags, &mut blobs, &mut trees, &mut commits] {
            group.sort_unstable_by(|a, b| b.name_hash.cmp(&a.name_hash).then(a.id.cmp(&b.id)));
            out.extend(group.iter().map(|found| found.id));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gib_object::{ObjectType, RawObject};
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    /// An object store in memory, so the walk's own behaviour can be exercised
    /// without a repository on disk.
    #[derive(Default)]
    struct Store {
        objects: BTreeMap<ObjectId, RawObject>,
        asked: RefCell<Vec<ObjectId>>,
    }

    impl Store {
        /// Add an object, returning the id git would give it.
        fn add(&mut self, object_type: ObjectType, body: Vec<u8>) -> ObjectId {
            let raw = RawObject { object_type, body };
            let id = raw.compute_id();
            self.objects.insert(id, raw);
            id
        }

        fn blob(&mut self, content: &str) -> ObjectId {
            self.add(ObjectType::Blob, content.as_bytes().to_vec())
        }

        /// A tree of `(mode, name, id)` entries, in the order given — which the
        /// caller must keep sorted the way git sorts them, since nothing here
        /// re-sorts them.
        fn tree(&mut self, entries: &[(&str, &str, ObjectId)]) -> ObjectId {
            let mut body = Vec::new();
            for (mode, name, id) in entries {
                body.extend_from_slice(mode.as_bytes());
                body.push(b' ');
                body.extend_from_slice(name.as_bytes());
                body.push(0);
                body.extend_from_slice(id.bytes());
            }
            self.add(ObjectType::Tree, body)
        }

        fn commit(&mut self, tree: ObjectId, parents: &[ObjectId], message: &str) -> ObjectId {
            let mut body = format!("tree {tree}\n");
            for parent in parents {
                body.push_str(&format!("parent {parent}\n"));
            }
            body.push_str(
                "author A <a@example.org> 1700000000 +0000\n\
                 committer A <a@example.org> 1700000000 +0000\n\n",
            );
            body.push_str(message);
            self.add(ObjectType::Commit, body.into_bytes())
        }

        fn tag(&mut self, target: ObjectId, target_type: &str, name: &str) -> ObjectId {
            let body = format!(
                "object {target}\n\
                 type {target_type}\n\
                 tag {name}\n\
                 tagger A <a@example.org> 1700000000 +0000\n\n\
                 tagged\n"
            );
            self.add(ObjectType::Tag, body.into_bytes())
        }
    }

    impl ObjectSource for Store {
        fn object(&self, id: ObjectId) -> LocalBoxFuture<'_, anyhow::Result<Object>> {
            self.asked.borrow_mut().push(id);
            let found = self.objects.get(&id).map(|raw| RawObject {
                object_type: raw.object_type,
                body: raw.body.clone(),
            });
            async move {
                let raw = found.ok_or_else(|| anyhow::anyhow!("missing object {id}"))?;
                Object::from_raw(id, raw).map_err(|e| anyhow::anyhow!("{e:?}"))
            }
            .boxed_local()
        }
    }

    /// Two commits, the second changing one of two files, plus a subdirectory
    /// that never changes. Returns the store and the tip commit.
    fn history() -> (Store, ObjectId) {
        let mut store = Store::default();
        let shared = store.blob("shared\n");
        let sub = store.tree(&[("100644", "shared.txt", shared)]);
        let first = store.blob("one\n");
        let root_one = store.tree(&[
            ("100644", "file.txt", first),
            ("40000", "sub", sub),
            // A submodule: a commit id in a repository we don't have.
            ("160000", "vendor", ObjectId::from_hex(&[b'a'; 40]).unwrap()),
        ]);
        let one = store.commit(root_one, &[], "one\n");
        let second = store.blob("two\n");
        let root_two = store.tree(&[
            ("100644", "file.txt", second),
            ("40000", "sub", sub),
            ("160000", "vendor", ObjectId::from_hex(&[b'a'; 40]).unwrap()),
        ]);
        let two = store.commit(root_two, &[one], "two\n");
        (store, two)
    }

    fn collect(store: &Store, tips: &[ObjectId]) -> Vec<ObjectId> {
        futures::executor::block_on(collect_objects(store, tips, &|_, _| {}))
            .expect("the walk succeeds")
    }

    /// Every object of every revision, each exactly once: two commits, three
    /// trees (one of them shared between the revisions), and three blobs.
    #[test]
    fn walks_the_whole_history() {
        let (store, tip) = history();
        let objects = collect(&store, &[tip]);

        assert_eq!(objects.len(), 8, "{objects:?}");
        let unique: BTreeSet<_> = objects.iter().collect();
        assert_eq!(unique.len(), objects.len(), "an object was packed twice");
    }

    /// A submodule's commit is named by a tree entry but lives elsewhere;
    /// fetching it would fail, and packing it is not ours to do.
    #[test]
    fn leaves_submodule_commits_alone() {
        let (store, tip) = history();
        let gitlink = ObjectId::from_hex(&[b'a'; 40]).unwrap();

        let objects = collect(&store, &[tip]);
        assert!(!objects.contains(&gitlink));
        assert!(!store.asked.borrow().contains(&gitlink));
    }

    /// The whole point of taking ids rather than objects: a blob's id comes off
    /// its tree entry, so the walk never reads one. They are fetched once, when
    /// the pack is written.
    #[test]
    fn never_fetches_a_blob() {
        let (store, tip) = history();
        let objects = collect(&store, &[tip]);
        let asked = store.asked.borrow().clone();

        let blobs: Vec<_> = objects
            .iter()
            .filter(|id| store.objects[id].object_type == ObjectType::Blob)
            .collect();
        assert_eq!(blobs.len(), 3);
        for blob in blobs {
            assert!(!asked.contains(blob), "the walk fetched blob {blob}");
        }
    }

    /// git's packing order: tags, then blobs, then trees, then commits. Like
    /// objects sit together, which is what gives the delta window in the writer
    /// anything to work with.
    #[test]
    fn packs_in_gits_order() {
        let (store, tip) = history();
        let objects = collect(&store, &[tip]);

        let types: Vec<ObjectType> = objects
            .iter()
            .map(|id| store.objects[id].object_type)
            .collect();
        let rank = |t: &ObjectType| match t {
            ObjectType::Tag => 0,
            ObjectType::Blob => 1,
            ObjectType::Tree => 2,
            ObjectType::Commit => 3,
        };
        let mut sorted = types.clone();
        sorted.sort_by_key(rank);
        assert_eq!(types, sorted, "objects are not in git's type order");

        // The same walk twice is the same list: nothing in the order comes from
        // what happened to be fetched first.
        assert_eq!(collect(&store, &[tip]), objects);
    }

    /// The reason the order is worth having: a file's revisions are adjacent,
    /// however far apart in history they were made.
    #[test]
    fn groups_a_path_s_revisions_together() {
        let mut store = Store::default();
        // Two files, each with two revisions, committed so that the walk meets
        // them interleaved rather than grouped.
        let (one_a, one_b) = (store.blob("one v1\n"), store.blob("one v2\n"));
        let (two_a, two_b) = (store.blob("two v1\n"), store.blob("two v2\n"));
        let first = store.tree(&[
            ("100644", "alpha.txt", one_a),
            ("100644", "beta.txt", two_a),
        ]);
        let second = store.tree(&[
            ("100644", "alpha.txt", one_b),
            ("100644", "beta.txt", two_b),
        ]);
        let c1 = store.commit(first, &[], "one\n");
        let c2 = store.commit(second, &[c1], "two\n");

        let objects = collect(&store, &[c2]);
        let at = |id: ObjectId| objects.iter().position(|&o| o == id).unwrap();
        // Each path's two blobs are neighbours.
        assert_eq!(at(one_a).abs_diff(at(one_b)), 1, "{objects:?}");
        assert_eq!(at(two_a).abs_diff(at(two_b)), 1, "{objects:?}");
    }

    /// git's `pack_name_hash`, pinned to values computed from the C by hand:
    /// the ordering is only worth anything if this agrees with git's about what
    /// counts as a similar name.
    #[test]
    fn name_hashes_weigh_the_end_of_the_path() {
        assert_eq!(name_hash(b""), 0);
        assert_eq!(name_hash(b"README.md"), 0x8397_7600);
        assert_eq!(name_hash(b"src/render/snapshot.rs"), 0x94c2_74ff);
        // Only the last sixteen non-space characters really count, so a file
        // and its namesake in another directory hash close together.
        assert_eq!(name_hash(b"snapshot.rs"), 0x94c2_73b0);
        // Whitespace is skipped rather than hashed.
        assert_eq!(name_hash(b"a b.rs"), name_hash(b"ab.rs"));
    }

    /// The walk never builds a path string: it carries the hash and extends it.
    /// That is only valid because the hash folds left, which this states.
    #[test]
    fn extending_a_hash_matches_hashing_the_whole_path() {
        let parent = name_hash(b"src/render");
        let whole = name_hash(b"src/render/snapshot.rs");
        assert_eq!(
            extend_hash(extend_hash(parent, b"/"), b"snapshot.rs"),
            whole
        );
    }

    /// An annotated tag is an object of its own, and packing it is what makes a
    /// clone of the bundle have the tag rather than just its commit.
    #[test]
    fn packs_the_tag_object_and_what_it_points_at() {
        let (mut store, tip) = history();
        let tag = store.tag(tip, "commit", "v1.0.0");

        let objects = collect(&store, &[tag]);
        assert!(objects.contains(&tag));
        assert!(objects.contains(&tip));
        // The tag rides along with the whole history, not just its commit.
        assert_eq!(objects.len(), 9);
    }

    /// A tag may point straight at a blob. There is no tree entry to learn its
    /// type from, so this is the one path where the walk does fetch a blob.
    #[test]
    fn packs_a_tag_of_a_blob() {
        let mut store = Store::default();
        let blob = store.blob("just a file\n");
        let tag = store.tag(blob, "blob", "v-blob");

        assert_eq!(collect(&store, &[tag]), vec![tag, blob]);
    }

    /// Two tips sharing history pack the shared objects once.
    #[test]
    fn merges_overlapping_tips() {
        let (store, tip) = history();
        let root = store.objects[&tip]
            .body
            .split(|&b| b == b'\n')
            .find_map(|line| line.strip_prefix(b"tree "))
            .and_then(ObjectId::from_hex)
            .unwrap();

        let both = collect(&store, &[tip, root]);
        assert_eq!(both, collect(&store, &[tip]));
    }

    /// A tip that isn't in the store is an error, not a bundle missing the
    /// objects a clone of it would need.
    #[test]
    fn a_missing_object_fails_the_walk() {
        let (store, _) = history();
        let missing = ObjectId::from_hex(&[b'b'; 40]).unwrap();

        let err = futures::executor::block_on(collect_objects(&store, &[missing], &|_, _| {}))
            .expect_err("the walk fails");
        assert!(err.to_string().contains(&missing.to_string()), "{err}");
    }

    /// The counts the caller draws a progress bar from: every fetch is
    /// eventually accounted for, and blobs are in neither number.
    #[test]
    fn reports_progress_up_to_the_objects_it_fetches() {
        let (store, tip) = history();
        let last = std::cell::Cell::new((0, 0));
        let objects =
            futures::executor::block_on(collect_objects(&store, &[tip], &|f, r| last.set((f, r))))
                .unwrap();

        let (fetched, requested) = last.get();
        assert_eq!(fetched, requested, "the walk ended mid-fetch");
        assert_eq!(fetched, store.asked.borrow().len());
        // Five of the eight objects are commits and trees; the other three are
        // blobs, which are never requested.
        assert_eq!(fetched, 5);
        assert_eq!(objects.len(), 8);
    }
}
