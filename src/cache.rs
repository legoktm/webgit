use crate::fs::HttpFilesystem;
use git_async::Repo;
use git_async::commit_graph::bloom::{BloomSettings, path_maybe_changed};
use git_async::diff::TreeDiff;
use git_async::error::{Error as GitError, GResult};
use git_async::object::{Commit, Object, ObjectId, ObjectType, RawObject, Tree};
use git_async::reference::{Ref, RefEntry, RefName};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{IdbDatabase, IdbOpenDbRequest, IdbRequest, IdbTransactionMode};

const DB_NAME: &str = "webgit";
const DB_VERSION: u32 = 5;
const STORE_OBJECTS: &str = "objects";
const STORE_TAG_REFS: &str = "tag_refs";
/// Per-commit commit-graph records, keyed by `"{repo_url}::{oid}"`. Content is a
/// pure function of the commit, so these are immutable and survive the server
/// regenerating its commit-graph (only genuinely new commits are ever missing).
const STORE_GRAPH: &str = "graph";

/// What the history walk needs about one commit, derived from the commit-graph
/// and cached per OID. `bloom` is the changed-path filter (`None` ⇒ treat as
/// "maybe", i.e. fall back to a real diff).
pub(crate) struct GraphRecord {
    pub(crate) tree: ObjectId,
    pub(crate) parents: Vec<ObjectId>,
    pub(crate) commit_time: i64,
    pub(crate) bloom: Option<Vec<u8>>,
}

// ---------------------------------------------------------------------------
// CachingRepo
// ---------------------------------------------------------------------------

#[derive(Copy, Clone)]
pub(crate) enum ClearTarget {
    RepoObjects,
    AllObjects,
}

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
            return Object::from_raw(id, raw);
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
        Object::from_raw(id, raw)
    }

    // --- Re-implementations that call lookup_object internally ---------------
    // These mirror the git_async methods of the same name but route every
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
    pub(crate) async fn graph_record(&self, id: ObjectId) -> Option<Rc<GraphRecord>> {
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

    /// Like [`graph_record`](Self::graph_record) but never triggers the
    /// whole-file bulk load. It serves from the in-memory session map (which a
    /// prior per-file walk may already have populated in full) and otherwise
    /// does a single targeted read of just this commit's record.
    ///
    /// Used by the unfiltered log walk (plain log / summary), which stops after
    /// one page of commits and so must not pull the entire graph just to show
    /// the latest few. It deliberately does **not** persist to IndexedDB: the
    /// bulk loader treats a non-empty store as "fully seeded", so writing
    /// isolated records here would make a later per-file walk skip the bulk load
    /// and miss most of history.
    pub(crate) async fn graph_record_lazy(&self, id: ObjectId) -> Option<Rc<GraphRecord>> {
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
        Some(rec)
    }

    /// Whether `bloom` (a commit's changed-path filter) definitively says the
    /// path did not change. `false` means "unknown" — no filter or a possible
    /// match — so the caller must diff.
    pub(crate) fn graph_path_unchanged(&self, bloom: Option<&[u8]>, path: &str) -> bool {
        let (Some(bytes), Some(settings)) = (bloom, self.graph_settings) else {
            return false;
        };
        !path_maybe_changed(bytes, &settings, path.as_bytes())
    }

    /// Populate the in-memory graph map once per session. Prefers the persisted
    /// per-commit records; if there are none (cold cache), bulk-loads the whole
    /// commit-graph in one request and persists every commit for next time.
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

    pub(crate) async fn clear_cache(&self, target: ClearTarget) {
        match target {
            ClearTarget::RepoObjects => {
                self.clear_store_by_prefix(STORE_OBJECTS).await;
                self.clear_store_by_prefix(STORE_GRAPH).await;
            }
            ClearTarget::AllObjects => {
                self.clear_store(STORE_OBJECTS).await;
                self.clear_store(STORE_GRAPH).await;
            }
        }
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

    async fn clear_store_by_prefix(&self, store_name: &str) {
        let Some(db) = self.db.as_ref() else { return };
        let prefix = format!("{}::", self.repo_url);

        let keys: Vec<String> = {
            let Ok(tx) = db.transaction_with_str(store_name) else {
                return;
            };
            let Ok(store) = tx.object_store(store_name) else {
                return;
            };
            let Ok(req) = store.get_all_keys() else {
                return;
            };
            let Ok(result) = await_request(&req).await else {
                return;
            };
            let arr = js_sys::Array::from(&result);
            (0..arr.length())
                .filter_map(|i| arr.get(i).as_string())
                .filter(|k| k.starts_with(&prefix))
                .collect()
        };

        if keys.is_empty() {
            return;
        }

        // Queue all deletes synchronously so the transaction stays open,
        // then await only the last request.
        let Ok(tx) = db.transaction_with_str_and_mode(store_name, IdbTransactionMode::Readwrite)
        else {
            return;
        };
        let Ok(store) = tx.object_store(store_name) else {
            return;
        };
        let mut last_req = None;
        for key in &keys {
            if let Ok(req) = store.delete(&JsValue::from_str(key)) {
                last_req = Some(req);
            }
        }
        if let Some(req) = last_req {
            await_request(&req).await.ok();
        }
    }

    // --- Stats ---------------------------------------------------------------

    /// Returns `(repo_objects, repo_mb, global_objects, global_mb)`,
    /// or `None` if IndexedDB is unavailable.
    pub(crate) async fn about_stats(&self) -> Option<(usize, f64, usize, f64)> {
        self.object_store_stats().await
    }

    async fn object_store_stats(&self) -> Option<(usize, f64, usize, f64)> {
        let db = self.db.as_ref()?;
        let tx = db.transaction_with_str(STORE_OBJECTS).ok()?;
        let store = tx.object_store(STORE_OBJECTS).ok()?;
        let req = store.get_all().ok()?;
        let result = await_request(&req).await.ok()?;
        let arr = js_sys::Array::from(&result);
        let prefix = format!("{}::", self.repo_url);
        let mut repo_count = 0usize;
        let mut repo_bytes = 0u64;
        let mut global_bytes = 0u64;
        for i in 0..arr.length() {
            let record = arr.get(i);
            let is_repo = js_sys::Reflect::get(&record, &"id".into())
                .ok()
                .and_then(|v| v.as_string())
                .is_some_and(|k| k.starts_with(&prefix));
            if let Ok(data) = js_sys::Reflect::get(&record, &"data".into())
                && let Ok(ab) = data.dyn_into::<js_sys::ArrayBuffer>()
            {
                let bytes = ab.byte_length() as u64;
                global_bytes += bytes;
                if is_repo {
                    repo_count += 1;
                    repo_bytes += bytes;
                }
            }
        }
        let global_count = arr.length() as usize;
        Some((
            repo_count,
            repo_bytes as f64 / (1024.0 * 1024.0),
            global_count,
            global_bytes as f64 / (1024.0 * 1024.0),
        ))
    }

    // --- IndexedDB helpers ---------------------------------------------------

    async fn idb_get(&self, id: ObjectId) -> Option<RawObject> {
        let db = self.db.as_ref()?;
        let key = format!("{}::{}", self.repo_url, id);
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
        let key = format!("{}::{}", self.repo_url, id);
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

    /// Load every persisted graph record for this repo (one `getAll`). Records
    /// whose Bloom-settings tag no longer matches keep their (immutable)
    /// metadata but drop the filter, so a settings change can't mislead.
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
        let Ok(req) = store.get_all() else {
            return out;
        };
        let Ok(result) = await_request(&req).await else {
            return out;
        };
        let arr = js_sys::Array::from(&result);
        let prefix = format!("{}::", self.repo_url);
        for i in 0..arr.length() {
            let record = arr.get(i);
            let Some(key) = js_sys::Reflect::get(&record, &"id".into())
                .ok()
                .and_then(|v| v.as_string())
            else {
                continue;
            };
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
            .chunks_exact(20)
            .filter_map(oid_from_bytes)
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

    /// Queue a single graph record write (fire-and-forget, like [`Self::idb_set`]).
    fn idb_set_graph(&self, id: ObjectId, rec: &GraphRecord) {
        let Some(db) = self.db.as_ref() else { return };
        let Ok(tx) = db.transaction_with_str_and_mode(STORE_GRAPH, IdbTransactionMode::Readwrite)
        else {
            return;
        };
        let Ok(store) = tx.object_store(STORE_GRAPH) else {
            return;
        };
        let js = self.build_graph_js(id, rec.tree, &rec.parents, rec.commit_time, rec.bloom.as_deref());
        store.put(&js).ok();
    }

    /// Persist every commit's record in one transaction, without awaiting it.
    /// The in-memory map is already populated for this session, so the walk need
    /// not wait; IndexedDB keeps the transaction alive until the queued writes
    /// commit on their own (for the next session).
    fn idb_bulk_put_graph(
        &self,
        records: &[(ObjectId, git_async::commit_graph::CommitGraphEntry, Option<Vec<u8>>)],
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
            let js =
                self.build_graph_js(*id, entry.tree, &entry.parents, entry.commit_time, bloom.as_deref());
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

/// A numeric fingerprint of the Bloom settings, stored beside each filter so a
/// settings change invalidates only the filters (not the metadata).
fn settings_tag(s: BloomSettings) -> f64 {
    f64::from((s.hash_version & 0xff) | ((s.num_hashes & 0xff) << 8) | ((s.bits_per_entry & 0xffff) << 16))
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
    js_sys::Reflect::get(record, &JsValue::from_str(key)).ok()?.as_f64()
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
                // filter), keyed like objects by "{repo_url}::{oid}".
                let params = web_sys::IdbObjectStoreParameters::new();
                params.set_key_path(&JsValue::from_str("id"));
                db.create_object_store_with_optional_parameters(STORE_GRAPH, &params)
                    .ok();
            }
        },
    );
    open_req.set_onupgradeneeded(Some(upgrade_cb.as_ref().unchecked_ref()));
    upgrade_cb.forget();

    let result = await_request(open_req.as_ref()).await?;
    result.dyn_into::<IdbDatabase>()
}

// ---------------------------------------------------------------------------
// Async wrapper for IdbRequest
// ---------------------------------------------------------------------------

async fn await_request(req: &IdbRequest) -> Result<JsValue, JsValue> {
    let req = req.clone();
    let promise = js_sys::Promise::new(&mut |resolve, reject| {
        {
            let req2 = req.clone();
            let resolve = resolve.clone();
            let cb = Closure::<dyn FnMut()>::new(move || {
                let val = req2.result().unwrap_or(JsValue::UNDEFINED);
                resolve.call1(&JsValue::UNDEFINED, &val).ok();
            });
            req.set_onsuccess(Some(cb.as_ref().unchecked_ref()));
            cb.forget();
        }
        {
            let req = req.clone();
            let cb = Closure::<dyn FnMut()>::new(move || {
                reject.call0(&JsValue::UNDEFINED).ok();
            });
            req.set_onerror(Some(cb.as_ref().unchecked_ref()));
            cb.forget();
        }
    });
    JsFuture::from(promise).await
}
