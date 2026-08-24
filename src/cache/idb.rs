//! Opening the database, migrating it between versions, and the async wrapper
//! that turns an `IdbRequest`'s callback pair into a future.
//!
//! All of it is plumbing: nothing above this module should have to know that
//! IndexedDB hands its answers back through `onsuccess`/`onerror`.

use super::{DB_NAME, DB_VERSION, STORE_GRAPH, STORE_OBJECTS, STORE_TAG_REFS};
use std::cell::RefCell;
use std::rc::Rc;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{IdbDatabase, IdbOpenDbRequest, IdbRequest};

/// Why the open failed when another tab pins the database at an older version.
/// Worth spelling out: it is the one cache failure the user can actually fix.
const BLOCKED_MESSAGE: &str = "upgrade blocked by another webgit tab holding an older version of the database; \
     close that tab and reload to re-enable caching";

pub(super) async fn open_db() -> Result<IdbDatabase, JsValue> {
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

pub(super) async fn await_request(req: &IdbRequest) -> Result<JsValue, JsValue> {
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
