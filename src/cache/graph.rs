//! The commit-graph accelerators, and the IndexedDB store behind them.
//!
//! A history walk needs each commit's tree, parents and time, and a verdict on
//! whether a path changed. Answering those from the commit-graph instead of the
//! commit object is the difference between a ranged request per commit and
//! none, so the records get a store of their own, keyed per repository.

use super::codec::{bytes_to_buf, get_bytes, get_number, oid_from_bytes, set_field};
use super::idb::await_request;
use super::keys::graph_key_bounds;
use super::{CachingRepo, STORE_GRAPH};
use gib::object::ObjectId;
use gib_log::GraphRecord;
use std::rc::Rc;
use wasm_bindgen::prelude::*;
use web_sys::IdbTransactionMode;

impl CachingRepo {
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
    pub(super) async fn lookup_graph_record(&self, id: ObjectId) -> Option<Rc<GraphRecord>> {
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
