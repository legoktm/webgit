//! The object cache: a [`Repo`] wrapper that answers every lookup from
//! IndexedDB when it can, and from the network when it cannot.
//!
//! The pieces live alongside: [`idb`] opens and migrates the database, [`codec`]
//! converts records to and from JS values, [`graph`] holds the commit-graph
//! accelerators and their store, [`global`] the repo-independent cache access,
//! and [`keys`] the two pure key computations.

mod codec;
mod global;
mod graph;
mod idb;
mod keys;

pub(crate) use global::GlobalCache;

use codec::{object_type_to_u8, settings_tag, u8_to_object_type};
use global::{cached_object_stats, clear_cached_objects};
use idb::{await_request, open_db};
use keys::resolve_prefix_in_map;

use crate::error::GitContext;
use crate::fs::HttpFilesystem;
use futures::FutureExt;
use futures::future::LocalBoxFuture;
use gib::Repo;
use gib::commit_graph::bloom::BloomSettings;
use gib::diff::TreeDiff;
use gib::error::{Error as GitError, GResult};
use gib::notes::{default_notes_ref, lookup_note};
use gib::object::{Commit, Object, ObjectId, ObjectIdPrefix, PrefixResolution, RawObject, Tree};
use gib::prelude::*;
use gib::reference::{Ref, RefEntry, RefName};
use gib_log::{CommitSource, GraphRecord};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use wasm_bindgen::prelude::*;
use web_sys::{IdbDatabase, IdbTransactionMode};

const DB_NAME: &str = "webgit";
const DB_VERSION: u32 = 6;
const STORE_OBJECTS: &str = "objects";
const STORE_TAG_REFS: &str = "tag_refs";
/// Per-commit commit-graph records, keyed by `"{repo_url}::{oid}"`. Content is a
/// pure function of the commit, so these are immutable and survive the server
/// regenerating its commit-graph (only genuinely new commits are ever missing).
const STORE_GRAPH: &str = "graph";

// ---------------------------------------------------------------------------
// CachingRepo
// ---------------------------------------------------------------------------

pub(crate) struct CachingRepo {
    inner: Repo<HttpFilesystem>,
    db: Option<IdbDatabase>,
    repo_url: String,
    /// Refs resolved once per session; navigation reuses the same snapshot.
    all_refs: RefCell<Option<Rc<BTreeMap<RefName, RefEntry>>>>,
    /// Changed-path Bloom settings of the commit-graph, and a numeric tag
    /// derived from them. Cached filter bytes are only trusted when their stored
    /// tag matches, so a settings change can never produce a false negative.
    graph_settings: Option<BloomSettings>,
    graph_tag: f64,
    /// In-memory per-commit graph records for this session, loaded once from
    /// IndexedDB (or, on an empty cache, from a single bulk fetch of the file).
    graph: RefCell<BTreeMap<ObjectId, Rc<GraphRecord>>>,
    graph_loaded: RefCell<bool>,
    /// Whether the persisted graph store holds a *complete* set of this repo's
    /// records: true once a bulk load has succeeded, or once a non-empty store
    /// was read back (which implies an earlier one did). Per-commit writes are
    /// suppressed until then — see [`idb_set_graph`](Self::idb_set_graph).
    graph_seeded: RefCell<bool>,
}

impl CachingRepo {
    pub(crate) async fn open(inner: Repo<HttpFilesystem>, repo_url: String) -> Self {
        let db = open_db()
            .await
            .inspect_err(|e| {
                web_sys::console::warn_2(
                    &"webgit: IndexedDB unavailable, caching disabled:".into(),
                    e,
                );
            })
            .ok();
        let graph_settings = inner.commit_graph().and_then(|g| g.bloom_settings());
        let graph_tag = graph_settings.map_or(0.0, settings_tag);
        Self {
            inner,
            db,
            repo_url,
            all_refs: RefCell::new(None),
            graph_settings,
            graph_tag,
            graph: RefCell::new(BTreeMap::new()),
            graph_loaded: RefCell::new(false),
            graph_seeded: RefCell::new(false),
        }
    }

    /// Whether IndexedDB-backed caching is active for this session.
    pub(crate) fn idb_available(&self) -> bool {
        self.db.is_some()
    }

    // --- Core cached lookup ---------------------------------------------------

    pub(crate) async fn lookup_object(&self, id: ObjectId) -> GResult<Object> {
        if let Some(raw) = self.idb_get(id).await {
            crate::fetch::record_cache_hit(raw.body.len() as u64);
            return Ok(Object::from_raw(id, raw)?);
        }
        let raw = self
            .inner
            .lookup_raw(id)
            .await?
            .ok_or(GitError::MissingObject(id))?;
        // Check the bytes against the ID we asked for before anything else
        // sees them. This is the one path where an object enters the process,
        // and it already costs a network round trip, so hashing is lost in the
        // noise; a cache hit above needs no check of its own, because nothing
        // that failed here was ever written to IndexedDB.
        raw.verify(id)?;
        // Queue the cache write but don't await it: the returned object doesn't
        // depend on the write completing, and IndexedDB keeps the transaction
        // alive until the queued `put` finishes on its own. Awaiting here would
        // make every caller block on a disk write it doesn't care about.
        self.idb_set(id, &raw);
        Ok(Object::from_raw(id, raw)?)
    }

    // --- Re-implementations that call lookup_object internally ---------------
    // These mirror the gib methods of the same name but route every
    // lookup through self.lookup_object so the cache is always consulted.

    pub(crate) async fn peel_ref_to_commit(&self, r: &Ref) -> GResult<Option<Commit>> {
        // resolve_object_id only follows ref-file chains, no object lookups.
        let oid = r.resolve_object_id(&self.inner).await?;
        let obj = self.lookup_object(oid).await?;
        self.peel_to_commit(&obj).await
    }

    pub(crate) async fn peel_to_commit(&self, obj: &Object) -> GResult<Option<Commit>> {
        let mut current = obj.clone();
        loop {
            match current {
                Object::Commit(c) => return Ok(Some(c)),
                Object::Tag(t) => current = self.lookup_object(t.target()).await?,
                _ => return Ok(None),
            }
        }
    }

    /// The note `refs/notes/commits` attaches to `id`, or `None` when the
    /// repository has no notes ref or none for this object.
    pub(crate) async fn note(&self, id: ObjectId) -> GResult<Option<Vec<u8>>> {
        let all_refs = self.all_refs().await?;
        let Some(entry) = all_refs.get(&default_notes_ref()) else {
            return Ok(None);
        };
        let obj = self.lookup_object(entry.commit_target()).await?;
        let Some(notes_commit) = self.peel_to_commit(&obj).await? else {
            return Ok(None);
        };
        let root = self.lookup_object(notes_commit.tree()).await?.tree()?;
        lookup_note(&root, id, async |id| self.lookup_object(id).await).await
    }

    /// Expand an abbreviated SHA (the kind commit messages quote) into the full
    /// object ID it names.
    ///
    /// Two sources are consulted, cheapest first:
    ///
    /// 1. The in-memory commit-graph map, which is sorted by OID, so a range
    ///    scan resolves an abbreviation with no I/O at all. It only holds
    ///    commits — exactly what a message reference names — and only once a
    ///    history view has loaded it, so a miss here is not authoritative. We
    ///    deliberately do *not* force [`ensure_graph_loaded`](Self::ensure_graph_loaded):
    ///    bulk-fetching the whole commit-graph would be a far heavier price
    ///    than the handful of ranged reads the pack indexes cost.
    /// 2. The pack indexes, via a binary search per pack.
    ///
    /// A hit in the graph wins even though some *other* object (a blob, or a
    /// commit newer than the commit-graph) might share the abbreviation: the
    /// only thing the caller can do with the answer is render a commit, so
    /// resolving to the commit is both what the author meant and the only
    /// useful outcome. Ambiguity *between commits* is still reported.
    pub(crate) async fn resolve_prefix(
        &self,
        prefix: &ObjectIdPrefix,
    ) -> GResult<PrefixResolution> {
        match resolve_prefix_in_map(&self.graph.borrow(), prefix) {
            PrefixResolution::NotFound => {}
            resolved => return Ok(resolved),
        }
        self.inner.resolve_prefix(prefix).await
    }

    pub(crate) async fn lookup_parents(&self, commit: &Commit) -> GResult<Vec<Commit>> {
        // Fetch a (merge) commit's parents concurrently rather than serially.
        let results =
            futures::future::join_all(commit.parents().iter().map(|&id| self.lookup_object(id)))
                .await;
        let mut out = Vec::with_capacity(results.len());
        for result in results {
            out.push(result?.commit()?);
        }
        Ok(out)
    }

    pub(crate) async fn tree_diff(&self, old: &Tree, new: &Tree) -> GResult<TreeDiff> {
        TreeDiff::new_with_lookup(old, new, async |id| self.lookup_object(id).await).await
    }

    // --- Delegators ----------------------------------------------------------

    pub(crate) async fn head(&self) -> GResult<Ref> {
        self.inner.head().await
    }

    /// All refs resolved to object IDs, fetched once and reused for the rest
    /// of the session.
    pub(crate) async fn all_refs(&self) -> GResult<Rc<BTreeMap<RefName, RefEntry>>> {
        if let Some(refs) = self.all_refs.borrow().as_ref() {
            return Ok(Rc::clone(refs));
        }
        let refs = Rc::new(self.inner.all_refs().await?);
        *self.all_refs.borrow_mut() = Some(Rc::clone(&refs));
        Ok(refs)
    }

    // --- Cache clearing ------------------------------------------------------

    /// Wipe the shared object cache and every repo's commit-graph records.
    /// Objects are keyed by bare OID (shared across repos), so there is no
    /// per-repo object cache to clear selectively.
    pub(crate) async fn clear_cache(&self) {
        clear_cached_objects(self.db.as_ref()).await;
        // Drop the in-memory graph so the next walk re-seeds from the (now empty)
        // store, bulk-loading the file again.
        self.graph.borrow_mut().clear();
        *self.graph_loaded.borrow_mut() = false;
    }

    // --- Stats ---------------------------------------------------------------

    /// Returns `(objects, size_mb)` for the shared object cache, or `None` if
    /// IndexedDB is unavailable. Objects are keyed by bare OID and shared across
    /// repos, so this is a single host-wide figure.
    pub(crate) async fn about_stats(&self) -> Option<(usize, f64)> {
        cached_object_stats(self.db.as_ref()).await
    }

    // --- IndexedDB helpers ---------------------------------------------------

    async fn idb_get(&self, id: ObjectId) -> Option<RawObject> {
        let db = self.db.as_ref()?;
        let key = id.to_string();
        let tx = db.transaction_with_str(STORE_OBJECTS).ok()?;
        let store = tx.object_store(STORE_OBJECTS).ok()?;
        let req = store.get(&JsValue::from_str(&key)).ok()?;
        let result = await_request(&req).await.ok()?;
        if result.is_undefined() || result.is_null() {
            return None;
        }
        let type_n = js_sys::Reflect::get(&result, &"type".into())
            .ok()?
            .as_f64()? as u8;
        let data = js_sys::Reflect::get(&result, &"data".into()).ok()?;
        Some(RawObject {
            object_type: u8_to_object_type(type_n)?,
            body: js_sys::Uint8Array::new(&data).to_vec(),
        })
    }

    /// Queue a write of `raw` into the object store without awaiting it.
    ///
    /// The synchronous JS work (building the record, copying the body into a
    /// JS buffer) happens now while `raw` is borrowed, but the `put` request is
    /// left to complete in the background. IndexedDB keeps the transaction open
    /// until the queued request finishes even after the Rust handles are
    /// dropped, so the write still commits.
    fn idb_set(&self, id: ObjectId, raw: &RawObject) {
        let Some(db) = self.db.as_ref() else { return };
        let Ok(tx) = db.transaction_with_str_and_mode(STORE_OBJECTS, IdbTransactionMode::Readwrite)
        else {
            return;
        };
        let Ok(store) = tx.object_store(STORE_OBJECTS) else {
            return;
        };

        let record = js_sys::Object::new();
        let key = id.to_string();
        js_sys::Reflect::set(&record, &"id".into(), &JsValue::from_str(&key)).ok();
        js_sys::Reflect::set(
            &record,
            &"type".into(),
            &JsValue::from_f64(object_type_to_u8(raw.object_type) as f64),
        )
        .ok();
        let buf = js_sys::Uint8Array::from(raw.body.as_slice()).buffer();
        js_sys::Reflect::set(&record, &"data".into(), &buf).ok();

        store.put(&record).ok();
    }
}

/// How [`gib_log`] reads this repository: every object through the cache, and
/// the commit-graph accelerators above. The walk itself lives in that crate;
/// this is only the wiring that tells it where the bytes come from.
impl CommitSource for CachingRepo {
    fn object(&self, id: ObjectId) -> LocalBoxFuture<'_, anyhow::Result<Object>> {
        async move { self.lookup_object(id).await.context("read object") }.boxed_local()
    }

    fn graph_record(&self, id: ObjectId) -> LocalBoxFuture<'_, Option<Rc<GraphRecord>>> {
        self.lookup_graph_record(id).boxed_local()
    }

    fn bloom_settings(&self) -> Option<BloomSettings> {
        self.graph_settings
    }
}
