//! The tree walk's own tests: ordering, concurrency bounds, progress and
//! the size cap, all driven through a fake object store that answers one
//! poll later than asked.

use super::*;
use gib_object::{ObjectType, RawObject};
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

/// An object store that answers from memory, one poll later than asked.
///
/// The delay is the point: a fetch that completed immediately would hide
/// whether the walk actually overlaps its requests, so every lookup yields
/// once before returning, and the source records how many were outstanding
/// at the high-water mark.
struct FakeSource {
    objects: BTreeMap<ObjectId, (ObjectType, Vec<u8>)>,
    live: Cell<usize>,
    peak: Cell<usize>,
    /// Every object in the order its fetch was first polled, which is when
    /// a real one would have put its request on the wire.
    started: RefCell<Vec<ObjectId>>,
}

impl FakeSource {
    fn new(objects: BTreeMap<ObjectId, (ObjectType, Vec<u8>)>) -> Self {
        Self {
            objects,
            live: Cell::new(0),
            peak: Cell::new(0),
            started: RefCell::new(Vec::new()),
        }
    }

    /// When `id` was first asked for, as a position in the fetch order.
    fn started_at(&self, id: ObjectId) -> usize {
        self.started
            .borrow()
            .iter()
            .position(|&started| started == id)
            .expect("object was fetched")
    }
}

impl ObjectSource for FakeSource {
    fn object(&self, id: ObjectId) -> LocalBoxFuture<'_, anyhow::Result<Object>> {
        async move {
            self.live.set(self.live.get() + 1);
            self.peak.set(self.peak.get().max(self.live.get()));
            self.started.borrow_mut().push(id);
            yield_once().await;
            self.live.set(self.live.get() - 1);
            let (object_type, body) = self
                .objects
                .get(&id)
                .ok_or_else(|| anyhow::anyhow!("missing object {id}"))?;
            Object::from_raw(
                id,
                RawObject {
                    object_type: *object_type,
                    body: body.clone(),
                },
            )
            .map_err(|e| anyhow::anyhow!("{e:?}"))
        }
        .boxed_local()
    }
}

/// Pend exactly once, waking immediately, so other queued futures get a
/// chance to run before this one finishes.
async fn yield_once() {
    let mut yielded = false;
    std::future::poll_fn(move |cx| {
        if yielded {
            std::task::Poll::Ready(())
        } else {
            yielded = true;
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        }
    })
    .await
}

fn oid(n: u8) -> ObjectId {
    ObjectId::from_bytes([n; 20])
}

/// Serialise tree entries into a git tree object body.
fn tree_body(entries: &[(&str, &str, u8)]) -> Vec<u8> {
    let mut body = Vec::new();
    for (mode, name, id) in entries {
        body.extend_from_slice(format!("{mode} {name}\0").as_bytes());
        body.extend_from_slice(&[*id; 20]);
    }
    body
}

struct Fixture {
    source: FakeSource,
    root: Tree,
}

/// A repository with a nested directory in the middle of its root, which is
/// what makes the ordering interesting: `src/`'s contents have to land
/// between `src` and the sibling that follows it.
fn fixture() -> Fixture {
    let root = tree_body(&[
        ("100644", "README.md", 10),
        ("40000", "src", 2),
        ("100755", "run.sh", 11),
        ("120000", "link.md", 12),
        ("160000", "vendor", 13),
    ]);
    let src = tree_body(&[("100644", "lib.rs", 14), ("40000", "render", 3)]);
    let render = tree_body(&[("100644", "mod.rs", 15)]);

    let mut objects = BTreeMap::new();
    objects.insert(oid(1), (ObjectType::Tree, root.clone()));
    objects.insert(oid(2), (ObjectType::Tree, src));
    objects.insert(oid(3), (ObjectType::Tree, render));
    objects.insert(oid(10), (ObjectType::Blob, b"hi\n".to_vec()));
    objects.insert(oid(11), (ObjectType::Blob, b"#!/bin/sh\n".to_vec()));
    objects.insert(oid(12), (ObjectType::Blob, b"README.md".to_vec()));
    objects.insert(oid(14), (ObjectType::Blob, b"pub mod x;\n".to_vec()));
    objects.insert(oid(15), (ObjectType::Blob, b"// mod\n".to_vec()));

    Fixture {
        root: Object::from_raw(
            oid(1),
            RawObject {
                object_type: ObjectType::Tree,
                body: root,
            },
        )
        .unwrap()
        .tree()
        .unwrap(),
        source: FakeSource::new(objects),
    }
}

fn collect(f: &Fixture) -> Vec<ArchiveEntry> {
    futures::executor::block_on(collect_entries(&f.source, &f.root, "", &|_, _| {})).unwrap()
}

/// The output is in depth-first tree order, with each directory's contents
/// between it and its next sibling — even though the walk itself expands
/// directories breadth-first and its fetches finish in whatever order they
/// please. Order comes from [`flatten`], not from the walk.
#[test]
fn test_walk_is_depth_first() {
    let f = fixture();
    let entries = collect(&f);
    let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "README.md",
            "src",
            "src/lib.rs",
            "src/render",
            "src/render/mod.rs",
            "run.sh",
            "link.md",
            "vendor",
        ]
    );
}

#[test]
fn test_walk_kinds_and_content() {
    let f = fixture();
    let entries = collect(&f);
    let by_path = |p: &str| entries.iter().find(|e| e.path == p).expect("entry present");

    assert_eq!(by_path("README.md").data, b"hi\n");
    assert_eq!(by_path("run.sh").kind, EntryKind::File { executable: true });
    assert_eq!(
        by_path("src").kind,
        EntryKind::Directory,
        "a directory entry carries no content of its own"
    );
    assert_eq!(
        by_path("link.md").kind,
        EntryKind::Symlink {
            target: b"README.md".to_vec()
        },
        "a symlink's blob is its target, not its content"
    );
}

/// A submodule is archived as an empty directory, the way `git archive`
/// writes one — and, unlike every other entry, is never fetched: the commit
/// it names lives in a repository this one doesn't have.
#[test]
fn test_walk_emits_submodule_as_empty_directory() {
    let f = fixture();
    let entries = collect(&f);
    let vendor = entries
        .iter()
        .find(|e| e.path == "vendor")
        .expect("the submodule is archived");
    assert_eq!(vendor.kind, EntryKind::Directory);
    assert!(vendor.data.is_empty());
    assert!(
        !f.source.started.borrow().contains(&oid(13)),
        "the submodule's commit must never be asked for"
    );
}

/// The point of the whole arrangement: requests overlap. Sequentially this
/// would never exceed one outstanding fetch.
#[test]
fn test_walk_overlaps_fetches() {
    let f = fixture();
    collect(&f);
    // The root queues three blobs and a subtree before anything is awaited,
    // so all four should be outstanding together.
    assert!(
        f.source.peak.get() >= 4,
        "expected overlapping fetches, peaked at {}",
        f.source.peak.get()
    );
}

/// Directories are expanded breadth-first, which is what keeps the blob
/// pool fed. The fixture is the shape that tells the two orders apart: a
/// deep, narrow chain of directories next to a wide one. Depth-first has to
/// walk the whole chain before it ever looks inside `wide`, leaving almost
/// nothing in flight the entire way down; breadth-first reaches both at the
/// same level and has `wide`'s files on the wire immediately.
#[test]
fn test_walk_expands_breadth_first() {
    let wide_blob = oid(30);
    let deep_blob = oid(31);
    let names: Vec<String> = (0..40).map(|i| format!("f-{i:02}")).collect();
    let wide: Vec<(&str, &str, u8)> = names.iter().map(|n| ("100644", n.as_str(), 30u8)).collect();

    let mut objects = BTreeMap::new();
    objects.insert(
        oid(1),
        (
            ObjectType::Tree,
            tree_body(&[("40000", "chain", 2), ("40000", "wide", 3)]),
        ),
    );
    objects.insert(oid(3), (ObjectType::Tree, tree_body(&wide)));
    // chain/c1/c2/c3/deep.txt — four levels before a single file.
    objects.insert(oid(2), (ObjectType::Tree, tree_body(&[("40000", "c1", 4)])));
    objects.insert(oid(4), (ObjectType::Tree, tree_body(&[("40000", "c2", 5)])));
    objects.insert(oid(5), (ObjectType::Tree, tree_body(&[("40000", "c3", 6)])));
    objects.insert(
        oid(6),
        (ObjectType::Tree, tree_body(&[("100644", "deep.txt", 31)])),
    );
    objects.insert(oid(30), (ObjectType::Blob, b"w\n".to_vec()));
    objects.insert(oid(31), (ObjectType::Blob, b"d\n".to_vec()));

    let source = FakeSource::new(objects);
    let root = Object::from_raw(
        oid(1),
        RawObject {
            object_type: ObjectType::Tree,
            body: tree_body(&[("40000", "chain", 2), ("40000", "wide", 3)]),
        },
    )
    .unwrap()
    .tree()
    .unwrap();

    let entries =
        futures::executor::block_on(collect_entries(&source, &root, "", &|_, _| {})).unwrap();
    assert_eq!(entries.len(), 40 + 6, "every entry is still archived");
    assert!(
        source.started_at(wide_blob) < source.started_at(deep_blob),
        "the wide directory's files should be requested before the chain is \
         walked to the bottom; wide started at {}, the deep file at {}",
        source.started_at(wide_blob),
        source.started_at(deep_blob),
    );
}

/// ...but not without limit: a wide directory still keeps the number of
/// outstanding requests bounded.
#[test]
fn test_walk_bounds_in_flight() {
    let count = MAX_IN_FLIGHT * 3;
    let names: Vec<String> = (0..count).map(|i| format!("file-{i:04}")).collect();
    let entries: Vec<(&str, &str, u8)> =
        names.iter().map(|n| ("100644", n.as_str(), 20u8)).collect();
    let body = tree_body(&entries);

    let mut objects = BTreeMap::new();
    objects.insert(oid(20), (ObjectType::Blob, b"x\n".to_vec()));
    let source = FakeSource::new(objects);
    let root = Object::from_raw(
        oid(1),
        RawObject {
            object_type: ObjectType::Tree,
            body,
        },
    )
    .unwrap()
    .tree()
    .unwrap();

    let entries =
        futures::executor::block_on(collect_entries(&source, &root, "", &|_, _| {})).unwrap();
    assert_eq!(entries.len(), count);
    assert!(
        source.peak.get() <= MAX_IN_FLIGHT,
        "{} fetches were outstanding at once, cap is {MAX_IN_FLIGHT}",
        source.peak.get()
    );
    // And the cap is a ceiling, not the working level: the pipeline should
    // sit near it rather than trickling.
    assert!(
        source.peak.get() > MAX_IN_FLIGHT / 2,
        "only {} fetches overlapped, well under the {MAX_IN_FLIGHT} cap",
        source.peak.get()
    );
}

/// What the snapshot view's bar is drawn from: counts that only ever go
/// up, that end with every requested object accounted for, and whose
/// denominator is still growing after objects have started landing — the
/// walk discovers the tree as it fetches it, so the total is not known in
/// advance and the bar has to tolerate it moving.
#[test]
fn test_walk_reports_progress() {
    let f = fixture();
    let ticks = std::cell::RefCell::new(Vec::new());
    let report = |fetched, queued| ticks.borrow_mut().push((fetched, queued));
    futures::executor::block_on(collect_entries(&f.source, &f.root, "", &report)).unwrap();
    let ticks = ticks.into_inner();

    // Two subtrees (src, render) and five blobs; the submodule is skipped
    // and the root tree was handed in already fetched.
    assert_eq!(
        ticks.last().copied(),
        Some((7, 7)),
        "the walk should finish with every requested object fetched"
    );
    for pair in ticks.windows(2) {
        let ((was_fetched, was_queued), (fetched, queued)) = (pair[0], pair[1]);
        assert!(
            fetched >= was_fetched && queued >= was_queued,
            "counts went backwards: {:?} then {:?}",
            pair[0],
            pair[1]
        );
        assert!(fetched <= queued, "fetched {fetched} of only {queued}");
    }
    assert!(
        ticks
            .windows(2)
            .any(|pair| pair[0].0 > 0 && pair[1].1 > pair[0].1),
        "expected the total to keep rising after objects began landing"
    );
}

/// Subtree fetches are bounded too, by their own budget: breadth-first
/// expansion reaches far more directories at once than depth-first ever
/// did, so a directory of directories must not put every one of them on the
/// wire together.
#[test]
fn test_walk_bounds_trees_in_flight() {
    let count = MAX_TREES_IN_FLIGHT * 3;
    let names: Vec<String> = (0..count).map(|i| format!("dir-{i:04}")).collect();
    let dirs: Vec<(&str, &str, u8)> = names.iter().map(|n| ("40000", n.as_str(), 2u8)).collect();
    let body = tree_body(&dirs);

    let mut objects = BTreeMap::new();
    // Every subdirectory is the same tree, holding one file.
    objects.insert(
        oid(2),
        (ObjectType::Tree, tree_body(&[("100644", "f", 20)])),
    );
    objects.insert(oid(20), (ObjectType::Blob, b"x\n".to_vec()));
    let source = FakeSource::new(objects);
    let root = Object::from_raw(
        oid(1),
        RawObject {
            object_type: ObjectType::Tree,
            body,
        },
    )
    .unwrap()
    .tree()
    .unwrap();

    let entries =
        futures::executor::block_on(collect_entries(&source, &root, "", &|_, _| {})).unwrap();
    assert_eq!(entries.len(), count * 2, "a directory and a file for each");
    // The two budgets are separate, so the ceiling is their sum.
    assert!(
        source.peak.get() <= MAX_IN_FLIGHT + MAX_TREES_IN_FLIGHT,
        "{} fetches were outstanding at once, cap is {}",
        source.peak.get(),
        MAX_IN_FLIGHT + MAX_TREES_IN_FLIGHT,
    );
    assert!(
        source.peak.get() > MAX_TREES_IN_FLIGHT,
        "only {} fetches overlapped; the tree and blob budgets should both \
         be in use at once",
        source.peak.get()
    );
}

/// A repository too big to archive fails with the size cap's message
/// rather than by exhausting memory.
#[test]
fn test_walk_enforces_size_cap() {
    let names: Vec<String> = (0..4).map(|i| format!("big-{i}")).collect();
    let entries: Vec<(&str, &str, u8)> =
        names.iter().map(|n| ("100644", n.as_str(), 21u8)).collect();
    let body = tree_body(&entries);

    let mut objects = BTreeMap::new();
    // Four of these overrun the cap; one does not.
    objects.insert(
        oid(21),
        (ObjectType::Blob, vec![0u8; MAX_ARCHIVE_BYTES / 3]),
    );
    let source = FakeSource::new(objects);
    let root = Object::from_raw(
        oid(1),
        RawObject {
            object_type: ObjectType::Tree,
            body,
        },
    )
    .unwrap()
    .tree()
    .unwrap();

    let err = futures::executor::block_on(collect_entries(&source, &root, "", &|_, _| {}))
        .expect_err("expected the size cap to reject this");
    assert!(
        err.to_string().contains("limit for archives"),
        "unexpected error: {err}"
    );
}

/// A missing object is reported with the path it was reached by, not just
/// its id.
#[test]
fn test_walk_reports_missing_subtree_path() {
    let mut f = fixture();
    f.source.objects.remove(&oid(2));
    let err = futures::executor::block_on(collect_entries(&f.source, &f.root, "", &|_, _| {}))
        .expect_err("expected the missing subtree to fail the walk");
    assert!(err.to_string().contains("src"), "unexpected error: {err}");
}
