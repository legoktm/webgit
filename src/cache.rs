use crate::fs::HttpFilesystem;
use git_async::Repo;
use git_async::error::{Error as GitError, GResult};
use git_async::object::{Commit, Object, ObjectId, ObjectType, RawObject};
use git_async::reference::{Ref, RefName};
use std::collections::BTreeSet;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{IdbDatabase, IdbOpenDbRequest, IdbRequest, IdbTransactionMode};

const DB_NAME: &str = "webgit";
const DB_VERSION: u32 = 1;
const STORE: &str = "objects";

// ---------------------------------------------------------------------------
// CachingRepo
// ---------------------------------------------------------------------------

pub(crate) struct CachingRepo {
    inner: Repo<HttpFilesystem>,
    db: Option<IdbDatabase>,
}

impl CachingRepo {
    pub(crate) async fn open(inner: Repo<HttpFilesystem>) -> Self {
        let db = open_db()
            .await
            .inspect_err(|e| {
                web_sys::console::warn_2(
                    &"webgit: IndexedDB unavailable, caching disabled:".into(),
                    e,
                );
            })
            .ok();
        Self { inner, db }
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

    // --- Delegators ----------------------------------------------------------

    pub(crate) async fn head(&self) -> GResult<Ref> {
        self.inner.head().await
    }

    pub(crate) async fn ref_names(&self) -> GResult<BTreeSet<RefName>> {
        self.inner.ref_names().await
    }

    pub(crate) async fn lookup_ref(&self, name: &RefName) -> GResult<Ref> {
        self.inner.lookup_ref(name).await
    }

    // --- IndexedDB helpers ---------------------------------------------------

    async fn idb_get(&self, id: ObjectId) -> Option<RawObject> {
        let db = self.db.as_ref()?;
        let tx = db.transaction_with_str(STORE).ok()?;
        let store = tx.object_store(STORE).ok()?;
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
        let Ok(tx) = db.transaction_with_str_and_mode(STORE, IdbTransactionMode::Readwrite) else {
            return;
        };
        let Ok(store) = tx.object_store(STORE) else {
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

    // Create the object store on first run / version bump.
    let upgrade_cb = Closure::<dyn FnMut(web_sys::IdbVersionChangeEvent)>::new(
        |event: web_sys::IdbVersionChangeEvent| {
            let req: IdbOpenDbRequest = event.target().unwrap().dyn_into().unwrap();
            let db: IdbDatabase = req.result().unwrap().dyn_into().unwrap();
            let params = web_sys::IdbObjectStoreParameters::new();
            params.set_key_path(&JsValue::from_str("oid"));
            db.create_object_store_with_optional_parameters(STORE, &params)
                .ok();
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
