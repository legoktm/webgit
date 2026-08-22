use crate::error::GitContext;
use crate::fs::HttpFilesystem;
use futures::FutureExt;
use futures::future::LocalBoxFuture;
use gib::Repo;
use gib::commit_graph::bloom::BloomSettings;
use gib::diff::TreeDiff;
use gib::error::{Error as GitError, GResult};
use gib::object::{
    Commit, Object, ObjectId, ObjectIdPrefix, ObjectType, PrefixResolution, RawObject, Tree,
};
use gib::prelude::*;
use gib::reference::{Ref, RefEntry, RefName};
use gib_log::{CommitSource, GraphRecord};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{IdbDatabase, IdbOpenDbRequest, IdbRequest, IdbTransactionMode};

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

    // --- Commit-graph accelerators ------------------------------------------
    // These consult the commit-graph cache (if any) so a history walk can get a
    // commit's tree/parents/time and a path's change verdict without fetching
    // and parsing the commit object or its trees. A `None`/`false` result means
    // "graph can't answer", and the caller falls back to reading objects.

    /// `(commit count, has changed-path filters)` for the repository's
    /// commit-graph, or `None` if it has none. For startup logging/diagnostics.
    pub(crate) fn commit_graph_info(&self) -> Option<(u32, bool)> {
        self.inner
            .commit_graph()
            .map(|g| (g.num_commits(), g.has_bloom()))
    }

    /// A commit's graph record, or `None` if the repository has no commit-graph
    /// or the commit isn't in it (the walk then falls back to the object).
    ///
    /// Served from the in-memory session map (populated once from IndexedDB, or
    /// by a single bulk fetch when the cache is empty). A genuinely new commit —
    /// one pushed since the cache was seeded — is resolved with a small range
    /// read of the live file and then cached in memory and IndexedDB.
    async fn lookup_graph_record(&self, id: ObjectId) -> Option<Rc<GraphRecord>> {
        self.ensure_graph_loaded().await;
        if let Some(rec) = self.graph.borrow().get(&id) {
            return Some(Rc::clone(rec));
        }
        let (entry, bloom) = self.inner.commit_graph()?.record(id).await.ok().flatten()?;
        let rec = Rc::new(GraphRecord {
            tree: entry.tree,
            parents: entry.parents,
            commit_time: entry.commit_time,
            bloom,
        });
        self.graph.borrow_mut().insert(id, Rc::clone(&rec));
        self.idb_set_graph(id, &rec);
        Some(rec)
    }

    /// Populate the in-memory graph map once per session. Prefers the persisted
    /// per-commit records; if there are none (cold cache), bulk-loads the whole
    /// commit-graph in one request and persists every commit for next time.
    ///
    /// A non-empty store has to mean "a bulk load finished", since that is
    /// exactly what the check below reads it as. Only a completed load may
    /// therefore leave records behind: a failed one must write nothing at all,
    /// or the stray records left by [`lookup_graph_record`](Self::lookup_graph_record)'s miss
    /// path would read as a finished load next session and suppress the bulk
    /// fetch forever. `graph_seeded` is what holds that line.
    async fn ensure_graph_loaded(&self) {
        if *self.graph_loaded.borrow() {
            return;
        }
        // Set before awaiting so a re-entrant call doesn't double-load; the walk
        // is sequential, so this just guards against pathological interleavings.
        *self.graph_loaded.borrow_mut() = true;

        let Some(cg) = self.inner.commit_graph() else {
            return;
        };

        let cached = self.idb_get_all_graph().await;
        if !cached.is_empty() {
            crate::console_log(&format!(
                "webgit: commit-graph: loaded {} cached commit records from IndexedDB",
                cached.len()
            ));
            let mut map = self.graph.borrow_mut();
            for (id, rec) in cached {
                map.insert(id, Rc::new(rec));
            }
            *self.graph_seeded.borrow_mut() = true;
            return;
        }

        if cg.num_commits() == 0 {
            return;
        }
        crate::console_log(&format!(
            "webgit: commit-graph: cache empty, bulk-loading whole file ({} commits)",
            cg.num_commits()
        ));
        let Ok(records) = cg.all_records().await else {
            // A transient failure here (a dropped connection, say) must leave
            // the store exactly as empty as it found it, so the next session
            // tries the bulk load again instead of inheriting a partial one.
            crate::console_log(
                "webgit: commit-graph: bulk load failed; this session will not persist records",
            );
            return;
        };
        {
            let mut map = self.graph.borrow_mut();
            for (id, entry, bloom) in &records {
                map.insert(
                    *id,
                    Rc::new(GraphRecord {
                        tree: entry.tree,
                        parents: entry.parents.clone(),
                        commit_time: entry.commit_time,
                        bloom: bloom.clone(),
                    }),
                );
            }
        }
        self.idb_bulk_put_graph(&records);
        *self.graph_seeded.borrow_mut() = true;
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
        self.clear_store(STORE_OBJECTS).await;
        self.clear_store(STORE_GRAPH).await;
        // Drop the in-memory graph so the next walk re-seeds from the (now empty)
        // store, bulk-loading the file again.
        self.graph.borrow_mut().clear();
        *self.graph_loaded.borrow_mut() = false;
    }

    async fn clear_store(&self, store_name: &str) {
        let Some(db) = self.db.as_ref() else { return };
        let Ok(tx) = db.transaction_with_str_and_mode(store_name, IdbTransactionMode::Readwrite)
        else {
            return;
        };
        let Ok(store) = tx.object_store(store_name) else {
            return;
        };
        if let Ok(req) = store.clear() {
            await_request(&req).await.ok();
        }
    }

    // --- Stats ---------------------------------------------------------------

    /// Returns `(objects, size_mb)` for the shared object cache, or `None` if
    /// IndexedDB is unavailable. Objects are keyed by bare OID and shared across
    /// repos, so this is a single host-wide figure.
    pub(crate) async fn about_stats(&self) -> Option<(usize, f64)> {
        let db = self.db.as_ref()?;
        let tx = db.transaction_with_str(STORE_OBJECTS).ok()?;
        let store = tx.object_store(STORE_OBJECTS).ok()?;
        let req = store.get_all().ok()?;
        let result = await_request(&req).await.ok()?;
        let arr = js_sys::Array::from(&result);
        let mut total_bytes = 0u64;
        for i in 0..arr.length() {
            let record = arr.get(i);
            if let Ok(data) = js_sys::Reflect::get(&record, &"data".into())
                && let Ok(ab) = data.dyn_into::<js_sys::ArrayBuffer>()
            {
                total_bytes += ab.byte_length() as u64;
            }
        }
        Some((
            arr.length() as usize,
            total_bytes as f64 / (1024.0 * 1024.0),
        ))
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

    // --- Commit-graph store helpers ------------------------------------------

    /// Load every persisted graph record for this repo (one `getAll`), asking
    /// IndexedDB for only this repo's key range so the records of every *other*
    /// cached repo are never deserialized into JS. Records whose Bloom-settings
    /// tag no longer matches keep their (immutable) metadata but drop the
    /// filter, so a settings change can't mislead.
    async fn idb_get_all_graph(&self) -> Vec<(ObjectId, GraphRecord)> {
        let mut out = Vec::new();
        let Some(db) = self.db.as_ref() else {
            return out;
        };
        let Ok(tx) = db.transaction_with_str(STORE_GRAPH) else {
            return out;
        };
        let Ok(store) = tx.object_store(STORE_GRAPH) else {
            return out;
        };
        let (prefix, upper) = graph_key_bounds(&self.repo_url);
        let Ok(range) =
            web_sys::IdbKeyRange::bound(&JsValue::from_str(&prefix), &JsValue::from_str(&upper))
        else {
            return out;
        };
        let Ok(req) = store.get_all_with_key(&range) else {
            return out;
        };
        let Ok(result) = await_request(&req).await else {
            return out;
        };
        let arr = js_sys::Array::from(&result);
        for i in 0..arr.length() {
            let record = arr.get(i);
            let Some(key) = js_sys::Reflect::get(&record, &"id".into())
                .ok()
                .and_then(|v| v.as_string())
            else {
                continue;
            };
            // The range is a *prefix* range, so it still admits a repo whose URL
            // begins with ours followed by "::"; re-checking the prefix and the
            // hex parse below keeps such a neighbour's records out.
            let Some(hex) = key.strip_prefix(&prefix) else {
                continue;
            };
            let Some(id) = ObjectId::from_hex(hex.as_bytes()) else {
                continue;
            };
            let Some(rec) = self.parse_graph_record(&record) else {
                continue;
            };
            out.push((id, rec));
        }
        out
    }

    fn parse_graph_record(&self, record: &JsValue) -> Option<GraphRecord> {
        let tree = oid_from_bytes(&get_bytes(record, "tree")?)?;
        let parents = get_bytes(record, "parents")
            .unwrap_or_default()
            .as_chunks::<20>()
            .0
            .iter()
            .filter_map(|parent| oid_from_bytes(parent.as_slice()))
            .collect();
        let commit_time = get_number(record, "time")? as i64;
        let tag = get_number(record, "tag").unwrap_or(f64::NAN);
        // Only trust the filter if it was written under the current settings.
        let bloom = (tag == self.graph_tag)
            .then(|| get_bytes(record, "bloom"))
            .flatten();
        Some(GraphRecord {
            tree,
            parents,
            commit_time,
            bloom,
        })
    }

    /// Queue a single graph record write (fire-and-forget, like [`Self::idb_set`]),
    /// but only into a store a bulk load has already filled.
    ///
    /// Writing single records into a store whose bulk load never completed
    /// would leave it non-empty but incomplete, and
    /// [`ensure_graph_loaded`](Self::ensure_graph_loaded) reads any non-empty
    /// store as a finished load — so those few records would suppress the bulk
    /// fetch in every future session, permanently costing one ranged request
    /// per commit walked.
    fn idb_set_graph(&self, id: ObjectId, rec: &GraphRecord) {
        if !*self.graph_seeded.borrow() {
            return;
        }
        let Some(db) = self.db.as_ref() else { return };
        let Ok(tx) = db.transaction_with_str_and_mode(STORE_GRAPH, IdbTransactionMode::Readwrite)
        else {
            return;
        };
        let Ok(store) = tx.object_store(STORE_GRAPH) else {
            return;
        };
        let js = self.build_graph_js(
            id,
            rec.tree,
            &rec.parents,
            rec.commit_time,
            rec.bloom.as_deref(),
        );
        store.put(&js).ok();
    }

    /// Persist every commit's record in one transaction, without awaiting it.
    /// The in-memory map is already populated for this session, so the walk need
    /// not wait; IndexedDB keeps the transaction alive until the queued writes
    /// commit on their own (for the next session).
    fn idb_bulk_put_graph(
        &self,
        records: &[(
            ObjectId,
            gib::commit_graph::CommitGraphEntry,
            Option<Vec<u8>>,
        )],
    ) {
        let Some(db) = self.db.as_ref() else { return };
        let Ok(tx) = db.transaction_with_str_and_mode(STORE_GRAPH, IdbTransactionMode::Readwrite)
        else {
            return;
        };
        let Ok(store) = tx.object_store(STORE_GRAPH) else {
            return;
        };
        for (id, entry, bloom) in records {
            let js = self.build_graph_js(
                *id,
                entry.tree,
                &entry.parents,
                entry.commit_time,
                bloom.as_deref(),
            );
            store.put(&js).ok();
        }
    }

    fn build_graph_js(
        &self,
        id: ObjectId,
        tree: ObjectId,
        parents: &[ObjectId],
        commit_time: i64,
        bloom: Option<&[u8]>,
    ) -> js_sys::Object {
        let record = js_sys::Object::new();
        let key = format!("{}::{}", self.repo_url, id);
        set_field(&record, "id", &JsValue::from_str(&key));
        set_field(&record, "tree", &bytes_to_buf(tree.bytes()));
        let mut parent_bytes = Vec::with_capacity(parents.len() * 20);
        for p in parents {
            parent_bytes.extend_from_slice(p.bytes());
        }
        set_field(&record, "parents", &bytes_to_buf(&parent_bytes));
        set_field(&record, "time", &JsValue::from_f64(commit_time as f64));
        match bloom {
            Some(bytes) => set_field(&record, "bloom", &bytes_to_buf(bytes)),
            None => set_field(&record, "bloom", &JsValue::NULL),
        }
        set_field(&record, "tag", &JsValue::from_f64(self.graph_tag));
        record
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

/// Resolve an abbreviated SHA against a map keyed by object ID. Sorted keys
/// mean the abbreviation covers one contiguous range, so the answer is the
/// first two entries of that range: none, exactly one, or more than one.
/// Free-standing and I/O-free so it can be unit-tested off the browser.
fn resolve_prefix_in_map<T>(
    map: &BTreeMap<ObjectId, T>,
    prefix: &ObjectIdPrefix,
) -> PrefixResolution {
    let mut matches = map.range(prefix.first()..=prefix.last()).map(|(id, _)| *id);
    match (matches.next(), matches.next()) {
        (None, _) => PrefixResolution::NotFound,
        (Some(id), None) => PrefixResolution::Found(id),
        (Some(_), Some(_)) => PrefixResolution::Ambiguous,
    }
}

/// Inclusive key bounds selecting one repo's records in the graph store, whose
/// keys are `"{repo_url}::{oid}"`. The lower bound is the bare prefix; the upper
/// appends U+FFFF, which sorts above every character a hex OID can contain, so
/// the range covers exactly the keys that continue the prefix. IndexedDB compares
/// strings by code point, so this needs no knowledge of the OID length.
/// Free-standing and I/O-free so it can be unit-tested off the browser.
fn graph_key_bounds(repo_url: &str) -> (String, String) {
    let prefix = format!("{repo_url}::");
    let upper = format!("{prefix}\u{ffff}");
    (prefix, upper)
}

/// A numeric fingerprint of the Bloom settings, stored beside each filter so a
/// settings change invalidates only the filters (not the metadata).
fn settings_tag(s: BloomSettings) -> f64 {
    f64::from(
        (s.hash_version & 0xff)
            | ((s.num_hashes & 0xff) << 8)
            | ((s.bits_per_entry & 0xffff) << 16),
    )
}

fn set_field(obj: &js_sys::Object, key: &str, value: &JsValue) {
    js_sys::Reflect::set(obj, &JsValue::from_str(key), value).ok();
}

fn bytes_to_buf(bytes: &[u8]) -> JsValue {
    js_sys::Uint8Array::from(bytes).buffer().into()
}

fn get_bytes(record: &JsValue, key: &str) -> Option<Vec<u8>> {
    let value = js_sys::Reflect::get(record, &JsValue::from_str(key)).ok()?;
    if value.is_undefined() || value.is_null() {
        return None;
    }
    Some(js_sys::Uint8Array::new(&value).to_vec())
}

fn get_number(record: &JsValue, key: &str) -> Option<f64> {
    js_sys::Reflect::get(record, &JsValue::from_str(key))
        .ok()?
        .as_f64()
}

fn oid_from_bytes(bytes: &[u8]) -> Option<ObjectId> {
    Some(ObjectId::from_bytes(<[u8; 20]>::try_from(bytes).ok()?))
}

// ---------------------------------------------------------------------------
// ObjectType ↔ u8 (matches git pack-file encoding)
// ---------------------------------------------------------------------------

fn object_type_to_u8(t: ObjectType) -> u8 {
    match t {
        ObjectType::Commit => 1,
        ObjectType::Tree => 2,
        ObjectType::Blob => 3,
        ObjectType::Tag => 4,
    }
}

fn u8_to_object_type(n: u8) -> Option<ObjectType> {
    match n {
        1 => Some(ObjectType::Commit),
        2 => Some(ObjectType::Tree),
        3 => Some(ObjectType::Blob),
        4 => Some(ObjectType::Tag),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// IndexedDB open
// ---------------------------------------------------------------------------

/// Why the open failed when another tab pins the database at an older version.
/// Worth spelling out: it is the one cache failure the user can actually fix.
const BLOCKED_MESSAGE: &str = "upgrade blocked by another webgit tab holding an older version of the database; \
     close that tab and reload to re-enable caching";

async fn open_db() -> Result<IdbDatabase, JsValue> {
    let window = web_sys::window().ok_or_else(|| JsValue::from_str("no window"))?;
    let factory = window
        .indexed_db()?
        .ok_or_else(|| JsValue::from_str("no indexedDB"))?;
    let open_req = factory.open_with_u32(DB_NAME, DB_VERSION)?;

    // Migrate the schema incrementally based on the previous version.
    let upgrade_cb = Closure::<dyn FnMut(web_sys::IdbVersionChangeEvent)>::new(
        |event: web_sys::IdbVersionChangeEvent| {
            let req: IdbOpenDbRequest = event.target().unwrap().dyn_into().unwrap();
            let db: IdbDatabase = req.result().unwrap().dyn_into().unwrap();
            let old = event.old_version() as u32;

            if old < 3 {
                // Recreate objects store keyed by "{repo_url}::{oid}" instead of bare "{oid}".
                // Existing cached objects are discarded; they will be re-fetched on demand.
                if old >= 1 {
                    db.delete_object_store(STORE_OBJECTS).ok();
                }
                let params = web_sys::IdbObjectStoreParameters::new();
                params.set_key_path(&JsValue::from_str("id"));
                db.create_object_store_with_optional_parameters(STORE_OBJECTS, &params)
                    .ok();
            }
            if old < 4 {
                // Tag refs are now resolved in bulk (info/refs or packed-refs)
                // once per session; the per-tag cache store is obsolete.
                if (2..4).contains(&old) {
                    db.delete_object_store(STORE_TAG_REFS).ok();
                }
            }
            if old < 5 {
                // Per-commit commit-graph records (metadata + changed-path Bloom
                // filter), keyed per-repo by "{repo_url}::{oid}".
                let params = web_sys::IdbObjectStoreParameters::new();
                params.set_key_path(&JsValue::from_str("id"));
                db.create_object_store_with_optional_parameters(STORE_GRAPH, &params)
                    .ok();
            }
            if old < 6 {
                // Objects are content-addressed, so the same OID is byte-for-byte
                // identical in every repo. Switch the key from "{repo_url}::{oid}"
                // to the bare OID so forks/mirrors share one cached copy. Versions
                // below 3 already used bare keys (and were wiped above), so only
                // 3..6 carry prefixed entries; rewrite those in place to keep the
                // cache warm across the upgrade.
                if old >= 3
                    && let Some(tx) = req.transaction()
                    && let Ok(store) = tx.object_store(STORE_OBJECTS)
                {
                    migrate_objects_drop_prefix(store);
                }
            }
        },
    );
    open_req.set_onupgradeneeded(Some(upgrade_cb.as_ref().unchecked_ref()));
    upgrade_cb.forget();

    let result = await_request(open_req.as_ref()).await?;
    let db: IdbDatabase = result.dyn_into()?;

    // Give up the connection as soon as another tab wants to upgrade the
    // schema. A connection held at the old version blocks that upgrade (see
    // the `blocked` arm of `await_request`) for as long as this page stays
    // open, and the other tab has no way to make us let go. Closing costs this
    // tab its cache — later transactions fail and every lookup falls through
    // to the network — which is far cheaper than stalling the other one.
    let on_version_change = Closure::<dyn FnMut()>::new({
        let db = db.clone();
        move || {
            web_sys::console::warn_1(
                &"webgit: another tab is upgrading the cache database; closing this connection"
                    .into(),
            );
            db.close();
        }
    });
    db.set_onversionchange(Some(on_version_change.as_ref().unchecked_ref()));
    // Outlives this function by design: it stays armed for the page's lifetime.
    on_version_change.forget();

    Ok(db)
}

/// Self-referential slot holding a cursor's `onsuccess` closure so it can re-arm
/// itself across `continue_()` calls; cleared to free the closure when done.
type CursorCallbackSlot = Rc<RefCell<Option<Closure<dyn FnMut(web_sys::Event)>>>>;

/// Rewrite every object key from `"{repo_url}::{oid}"` to the bare OID, in place,
/// inside the open versionchange transaction. Objects cached by several repos
/// collapse onto a single bare-OID entry (the `put` overwrites with identical
/// content), and already-bare keys are skipped, so the pass is idempotent.
fn migrate_objects_drop_prefix(store: web_sys::IdbObjectStore) {
    let Ok(req) = store.open_cursor() else { return };
    // The cursor walk is event-driven, so its success handler must re-arm itself
    // via `continue_()`. Park the closure in a cell that the closure also holds
    // (through a clone), forming a cycle that keeps it alive; clearing the cell
    // when iteration ends breaks the cycle so the closure is freed.
    let slot: CursorCallbackSlot = Rc::new(RefCell::new(None));
    let slot_inner = Rc::clone(&slot);
    let cb = Closure::<dyn FnMut(web_sys::Event)>::new(move |event: web_sys::Event| {
        let Some(req) = event.target().and_then(|t| t.dyn_into::<IdbRequest>().ok()) else {
            return;
        };
        let result = req.result().unwrap_or(JsValue::UNDEFINED);
        let Ok(cursor) = result.dyn_into::<web_sys::IdbCursorWithValue>() else {
            // Null/undefined cursor ⇒ iteration finished; drop the self-reference.
            slot_inner.borrow_mut().take();
            return;
        };
        if let Ok(value) = cursor.value()
            && let Some(key) = js_sys::Reflect::get(&value, &"id".into())
                .ok()
                .and_then(|v| v.as_string())
            && let Some(oid) = key.rsplit("::").next()
            && oid != key
        {
            // Re-key by writing the record under its bare OID and deleting the old
            // prefixed one (a cursor can't change its own inline key).
            js_sys::Reflect::set(&value, &"id".into(), &JsValue::from_str(oid)).ok();
            store.put(&value).ok();
            cursor.delete().ok();
        }
        cursor.continue_().ok();
    });
    req.set_onsuccess(Some(cb.as_ref().unchecked_ref()));
    *slot.borrow_mut() = Some(cb);
}

// ---------------------------------------------------------------------------
// Async wrapper for IdbRequest
// ---------------------------------------------------------------------------

/// Slot holding an in-flight request's handler closures, cleared by whichever
/// one fires. Same self-referential trick as [`CursorCallbackSlot`].
type RequestCallbackSlot = Rc<RefCell<Vec<Closure<dyn FnMut()>>>>;

/// Detach every handler from a settled request and drop them. Clearing the slot
/// breaks the cycle that kept the closures alive; unregistering first means the
/// request never holds a reference to a freed closure. Dropping the closure that
/// is currently running is fine — wasm-bindgen defers the actual free until the
/// call returns.
fn finish_request(req: &IdbRequest, slot: &RequestCallbackSlot) {
    req.set_onsuccess(None);
    req.set_onerror(None);
    if let Some(open_req) = req.dyn_ref::<IdbOpenDbRequest>() {
        open_req.set_onblocked(None);
    }
    slot.borrow_mut().clear();
}

async fn await_request(req: &IdbRequest) -> Result<JsValue, JsValue> {
    let req = req.clone();
    let promise = js_sys::Promise::new(&mut |resolve, reject| {
        // A request fires exactly one of its outcomes, so the handler that runs
        // can tear all of them down. Park them in a cell that the closures also
        // hold (through clones), forming a cycle that keeps them alive while the
        // request is in flight; whichever fires clears the cell, freeing them
        // all. Anything less leaks a closure per request, and there is one
        // request per object lookup.
        let slot: RequestCallbackSlot = Rc::new(RefCell::new(Vec::new()));
        let on_success = {
            let req = req.clone();
            let slot = Rc::clone(&slot);
            Closure::<dyn FnMut()>::new(move || {
                let val = req.result().unwrap_or(JsValue::UNDEFINED);
                resolve.call1(&JsValue::UNDEFINED, &val).ok();
                finish_request(&req, &slot);
            })
        };
        let on_error = {
            let req = req.clone();
            let slot = Rc::clone(&slot);
            let reject = reject.clone();
            Closure::<dyn FnMut()>::new(move || {
                reject.call0(&JsValue::UNDEFINED).ok();
                finish_request(&req, &slot);
            })
        };
        req.set_onsuccess(Some(on_success.as_ref().unchecked_ref()));
        req.set_onerror(Some(on_error.as_ref().unchecked_ref()));
        slot.borrow_mut().extend([on_success, on_error]);

        // Opening the database has a third outcome the others don't: `blocked`,
        // fired when another tab still holds a connection at the previous
        // `DB_VERSION`. The upgrade then waits for that tab to go away, firing
        // neither success nor error however long that takes, so waiting on
        // those two alone leaves the app hanging on a blank page indefinitely.
        // Fail the open instead: `CachingRepo::open` warns and runs uncached,
        // which is slower but is a working page.
        if let Some(open_req) = req.dyn_ref::<IdbOpenDbRequest>() {
            let on_blocked = {
                let req = req.clone();
                let slot = Rc::clone(&slot);
                Closure::<dyn FnMut()>::new(move || {
                    reject
                        .call1(&JsValue::UNDEFINED, &JsValue::from_str(BLOCKED_MESSAGE))
                        .ok();
                    finish_request(&req, &slot);
                })
            };
            open_req.set_onblocked(Some(on_blocked.as_ref().unchecked_ref()));
            slot.borrow_mut().push(on_blocked);
        }
    });
    JsFuture::from(promise).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An object ID written as a short hex string, right-padded with zeroes.
    fn oid(hex: &str) -> ObjectId {
        let mut padded = hex.as_bytes().to_vec();
        padded.resize(40, b'0');
        ObjectId::from_hex(&padded).unwrap()
    }

    fn prefix(hex: &str) -> ObjectIdPrefix {
        ObjectIdPrefix::from_hex(hex.as_bytes()).unwrap()
    }

    /// Stands in for the commit-graph map; only its keys matter here.
    fn map(ids: &[ObjectId]) -> BTreeMap<ObjectId, ()> {
        ids.iter().map(|id| (*id, ())).collect()
    }

    #[test]
    fn resolve_prefix_in_map_unique() {
        let map = map(&[oid("00"), oid("12ab34"), oid("12ff"), oid("ff")]);
        assert_eq!(
            resolve_prefix_in_map(&map, &prefix("12ab34")),
            PrefixResolution::Found(oid("12ab34"))
        );
        // An odd-length abbreviation covers half a byte's worth of IDs.
        assert_eq!(
            resolve_prefix_in_map(&map, &prefix("12ab3")),
            PrefixResolution::Found(oid("12ab34"))
        );
        // The first and last keys of the map.
        assert_eq!(
            resolve_prefix_in_map(&map, &prefix("0000")),
            PrefixResolution::Found(oid("00"))
        );
        assert_eq!(
            resolve_prefix_in_map(&map, &prefix("ff00")),
            PrefixResolution::Found(oid("ff"))
        );
    }

    #[test]
    fn resolve_prefix_in_map_not_found() {
        let map = map(&[oid("00"), oid("12ab34"), oid("ff")]);
        assert_eq!(
            resolve_prefix_in_map(&map, &prefix("12ac")),
            PrefixResolution::NotFound
        );
        assert_eq!(
            resolve_prefix_in_map(&map, &prefix("9999")),
            PrefixResolution::NotFound
        );
        assert_eq!(
            resolve_prefix_in_map(&BTreeMap::<ObjectId, ()>::new(), &prefix("12ab34")),
            PrefixResolution::NotFound
        );
    }

    #[test]
    fn resolve_prefix_in_map_ambiguous() {
        let map = map(&[oid("12ab34"), oid("12ab35"), oid("13")]);
        assert_eq!(
            resolve_prefix_in_map(&map, &prefix("12ab3")),
            PrefixResolution::Ambiguous
        );
        // The neighbouring `13…` key is outside the range and must not count.
        assert_eq!(
            resolve_prefix_in_map(&map, &prefix("12ab35")),
            PrefixResolution::Found(oid("12ab35"))
        );
    }

    #[test]
    fn graph_key_bounds_cover_one_repo() {
        let (lower, upper) = graph_key_bounds("https://example.org/a.git");
        let key = |repo: &str| format!("{repo}::{}", oid("12ab34"));
        let in_range = |k: &String| *k >= lower && *k <= upper;

        assert!(in_range(&key("https://example.org/a.git")));
        // A different repo, and one whose URL merely extends ours, are both out.
        assert!(!in_range(&key("https://example.org/b.git")));
        assert!(!in_range(&key("https://example.org/a.github")));
    }
}
