//! Cache access that belongs to no one repository: the object store is keyed
//! by bare OID and shared across every repo this browser has visited, so its
//! size and its "clear" button are host-wide.

use super::idb::{await_request, open_db};
use super::{STORE_GRAPH, STORE_OBJECTS};
use wasm_bindgen::JsCast;
use web_sys::{IdbDatabase, IdbTransactionMode};

pub(crate) struct GlobalCache {
    db: Option<IdbDatabase>,
}

impl GlobalCache {
    pub(crate) async fn open() -> Self {
        Self {
            db: open_db()
                .await
                .inspect_err(|e| {
                    web_sys::console::warn_2(&"webgit: IndexedDB unavailable:".into(), e);
                })
                .ok(),
        }
    }

    /// `(objects, size_mb)`, or `None` when IndexedDB is unavailable — the same
    /// figure [`CachingRepo::about_stats`](super::CachingRepo::about_stats) reports.
    pub(crate) async fn stats(&self) -> Option<(usize, f64)> {
        cached_object_stats(self.db.as_ref()).await
    }

    pub(crate) async fn clear(&self) {
        clear_cached_objects(self.db.as_ref()).await;
    }
}

/// Empty the shared object store and every repo's commit-graph records. A
/// caller holding in-memory copies of either has to drop those itself (see
/// [`CachingRepo::clear_cache`](super::CachingRepo::clear_cache)).
pub(super) async fn clear_cached_objects(db: Option<&IdbDatabase>) {
    let Some(db) = db else { return };
    for store_name in [STORE_OBJECTS, STORE_GRAPH] {
        let Ok(tx) = db.transaction_with_str_and_mode(store_name, IdbTransactionMode::Readwrite)
        else {
            continue;
        };
        let Ok(store) = tx.object_store(store_name) else {
            continue;
        };
        if let Ok(req) = store.clear() {
            await_request(&req).await.ok();
        }
    }
}

/// `(objects, size_mb)` for the shared object store, or `None` when there is no
/// database to read it from.
pub(super) async fn cached_object_stats(db: Option<&IdbDatabase>) -> Option<(usize, f64)> {
    let db = db?;
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
