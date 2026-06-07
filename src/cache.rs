use crate::console_log;
use crate::fs::HttpFilesystem;
use git_async::Repo;
use git_async::diff::TreeDiff;
use git_async::error::{Error as GitError, GResult};
use git_async::object::{Commit, Object, ObjectId, ObjectType, RawObject, Tree};
use git_async::reference::{Ref, RefName, RefTarget};
use std::collections::BTreeSet;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{IdbDatabase, IdbOpenDbRequest, IdbRequest, IdbTransactionMode};

const DB_NAME: &str = "webgit";
const DB_VERSION: u32 = 2;
const STORE_OBJECTS: &str = "objects";
const STORE_TAG_REFS: &str = "tag_refs";

// ---------------------------------------------------------------------------
// CachingRepo
// ---------------------------------------------------------------------------

pub(crate) struct CachingRepo {
    inner: Repo<HttpFilesystem>,
    db: Option<IdbDatabase>,
    repo_url: String,
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
        Self {
            inner,
            db,
            repo_url,
        }
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
        self.idb_set(id, &raw).await;
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

    pub(crate) async fn lookup_parents(&self, commit: &Commit) -> GResult<Vec<Commit>> {
        let mut out = Vec::with_capacity(commit.parents().len());
        for &id in commit.parents() {
            out.push(self.lookup_object(id).await?.commit()?);
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

    pub(crate) async fn ref_names(&self) -> GResult<BTreeSet<RefName>> {
        self.inner.ref_names().await
    }

    pub(crate) async fn lookup_ref(&self, name: &RefName) -> GResult<Ref> {
        // Tag refs are immutable: cache the name → OID mapping permanently.
        if let RefName::Ref(b) = name
            && let Some(short) = b.strip_prefix(b"tags/")
        {
            let short = String::from_utf8_lossy(short).into_owned();
            if let Some(oid) = self.tag_ref_get(&short).await {
                console_log(&format!("cache hit: tags/{short}"));
                return Ok(Ref::new_direct(name.clone(), oid));
            }
            let r = self.inner.lookup_ref(name).await?;
            if let RefTarget::Direct(oid) = r.target() {
                console_log(&format!("cache set: tags/{short}"));
                self.tag_ref_set(&short, *oid).await;
            }
            return Ok(r);
        }
        self.inner.lookup_ref(name).await
    }

    // --- Stats ---------------------------------------------------------------

    pub(crate) async fn about_stats(&self) -> Option<(usize, f64, usize)> {
        let (object_count, size_mb) = self.object_store_stats().await?;
        let tag_count = self.tag_ref_count().await.unwrap_or(0);
        Some((object_count, size_mb, tag_count))
    }

    async fn object_store_stats(&self) -> Option<(usize, f64)> {
        let db = self.db.as_ref()?;
        let tx = db.transaction_with_str(STORE_OBJECTS).ok()?;
        let store = tx.object_store(STORE_OBJECTS).ok()?;
        let req = store.get_all().ok()?;
        let result = await_request(&req).await.ok()?;
        let arr = js_sys::Array::from(&result);
        let count = arr.length() as usize;
        let mut total_bytes = 0u64;
        for i in 0..arr.length() {
            let record = arr.get(i);
            if let Ok(data) = js_sys::Reflect::get(&record, &"data".into())
                && let Ok(ab) = data.dyn_into::<js_sys::ArrayBuffer>()
            {
                total_bytes += ab.byte_length() as u64;
            }
        }
        Some((count, total_bytes as f64 / (1024.0 * 1024.0)))
    }

    async fn tag_ref_count(&self) -> Option<usize> {
        let db = self.db.as_ref()?;
        let tx = db.transaction_with_str(STORE_TAG_REFS).ok()?;
        let store = tx.object_store(STORE_TAG_REFS).ok()?;
        let req = store.count().ok()?;
        let result = await_request(&req).await.ok()?;
        Some(result.as_f64().unwrap_or(0.0) as usize)
    }

    // --- IndexedDB helpers ---------------------------------------------------

    async fn tag_ref_get(&self, short: &str) -> Option<ObjectId> {
        let db = self.db.as_ref()?;
        let key = format!("{}::tags/{}", self.repo_url, short);
        let tx = db.transaction_with_str(STORE_TAG_REFS).ok()?;
        let store = tx.object_store(STORE_TAG_REFS).ok()?;
        let req = store.get(&JsValue::from_str(&key)).ok()?;
        let result = await_request(&req).await.ok()?;
        if result.is_undefined() || result.is_null() {
            return None;
        }
        let oid_str = js_sys::Reflect::get(&result, &"oid".into())
            .ok()?
            .as_string()?;
        ObjectId::from_hex(oid_str.as_bytes())
    }

    async fn tag_ref_set(&self, short: &str, oid: ObjectId) {
        let Some(db) = self.db.as_ref() else { return };
        let key = format!("{}::tags/{}", self.repo_url, short);
        let Ok(tx) =
            db.transaction_with_str_and_mode(STORE_TAG_REFS, IdbTransactionMode::Readwrite)
        else {
            return;
        };
        let Ok(store) = tx.object_store(STORE_TAG_REFS) else {
            return;
        };
        let record = js_sys::Object::new();
        js_sys::Reflect::set(&record, &"id".into(), &JsValue::from_str(&key)).ok();
        js_sys::Reflect::set(&record, &"oid".into(), &JsValue::from_str(&oid.to_string())).ok();
        if let Ok(req) = store.put(&record) {
            await_request(&req).await.ok();
        }
    }

    async fn idb_get(&self, id: ObjectId) -> Option<RawObject> {
        let db = self.db.as_ref()?;
        let tx = db.transaction_with_str(STORE_OBJECTS).ok()?;
        let store = tx.object_store(STORE_OBJECTS).ok()?;
        let req = store.get(&JsValue::from_str(&id.to_string())).ok()?;
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

    async fn idb_set(&self, id: ObjectId, raw: &RawObject) {
        let Some(db) = self.db.as_ref() else { return };
        let Ok(tx) = db.transaction_with_str_and_mode(STORE_OBJECTS, IdbTransactionMode::Readwrite)
        else {
            return;
        };
        let Ok(store) = tx.object_store(STORE_OBJECTS) else {
            return;
        };

        let record = js_sys::Object::new();
        js_sys::Reflect::set(&record, &"oid".into(), &id.to_string().into()).ok();
        js_sys::Reflect::set(
            &record,
            &"type".into(),
            &JsValue::from_f64(object_type_to_u8(raw.object_type) as f64),
        )
        .ok();
        let buf = js_sys::Uint8Array::from(raw.body.as_slice()).buffer();
        js_sys::Reflect::set(&record, &"data".into(), &buf).ok();

        if let Ok(req) = store.put(&record) {
            await_request(&req).await.ok();
        }
    }
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

            if old < 1 {
                let params = web_sys::IdbObjectStoreParameters::new();
                params.set_key_path(&JsValue::from_str("oid"));
                db.create_object_store_with_optional_parameters(STORE_OBJECTS, &params)
                    .ok();
            }
            if old < 2 {
                let params = web_sys::IdbObjectStoreParameters::new();
                params.set_key_path(&JsValue::from_str("id"));
                db.create_object_store_with_optional_parameters(STORE_TAG_REFS, &params)
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
