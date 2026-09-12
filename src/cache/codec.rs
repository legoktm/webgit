//! Reading and writing the JS objects a cached record is stored as.
//!
//! IndexedDB holds plain JS values, so every field crosses the boundary as an
//! `ArrayBuffer` or a number; these are the conversions both directions.

use gib::commit_graph::bloom::BloomSettings;
use gib::object::{ObjectId, ObjectType, RawObject};
use wasm_bindgen::prelude::*;

/// A numeric fingerprint of the Bloom settings, stored beside each filter so a
/// settings change invalidates only the filters (not the metadata).
pub(super) fn settings_tag(s: BloomSettings) -> f64 {
    f64::from(
        (s.hash_version & 0xff)
            | ((s.num_hashes & 0xff) << 8)
            | ((s.bits_per_entry & 0xffff) << 16),
    )
}

pub(super) fn set_field(obj: &js_sys::Object, key: &str, value: &JsValue) {
    js_sys::Reflect::set(obj, &JsValue::from_str(key), value).ok();
}

pub(super) fn bytes_to_buf(bytes: &[u8]) -> JsValue {
    js_sys::Uint8Array::from(bytes).buffer().into()
}

pub(super) fn get_bytes(record: &JsValue, key: &str) -> Option<Vec<u8>> {
    let value = js_sys::Reflect::get(record, &JsValue::from_str(key)).ok()?;
    if value.is_undefined() || value.is_null() {
        return None;
    }
    Some(js_sys::Uint8Array::new(&value).to_vec())
}

pub(super) fn get_number(record: &JsValue, key: &str) -> Option<f64> {
    js_sys::Reflect::get(record, &JsValue::from_str(key))
        .ok()?
        .as_f64()
}

pub(super) fn oid_from_bytes(bytes: &[u8]) -> Option<ObjectId> {
    Some(ObjectId::from_bytes(<[u8; 20]>::try_from(bytes).ok()?))
}

/// One cached object as the object store holds it: the id it is keyed by, its
/// type as a number, and its body as an `ArrayBuffer`.
///
/// Written in one place and read in another — an object cached by a lookup is
/// read back by the next one, and an object written by a bundle import is read
/// back as a delta base — so both ends of the record's shape live here
/// together.
pub(super) fn object_record(id: ObjectId, raw: &RawObject) -> js_sys::Object {
    let record = js_sys::Object::new();
    set_field(&record, "id", &JsValue::from_str(&id.to_string()));
    set_field(
        &record,
        "type",
        &JsValue::from_f64(object_type_to_u8(raw.object_type) as f64),
    );
    set_field(&record, "data", &bytes_to_buf(&raw.body));
    record
}

/// The object a record holds, or `None` if the value isn't one — a miss, or a
/// record from a schema this build doesn't know.
pub(super) fn object_from_record(record: &JsValue) -> Option<RawObject> {
    if record.is_undefined() || record.is_null() {
        return None;
    }
    Some(RawObject {
        object_type: u8_to_object_type(get_number(record, "type")? as u8)?,
        body: get_bytes(record, "data")?,
    })
}

// ---------------------------------------------------------------------------
// ObjectType ↔ u8 (matches git pack-file encoding)
// ---------------------------------------------------------------------------

pub(super) fn object_type_to_u8(t: ObjectType) -> u8 {
    match t {
        ObjectType::Commit => 1,
        ObjectType::Tree => 2,
        ObjectType::Blob => 3,
        ObjectType::Tag => 4,
    }
}

pub(super) fn u8_to_object_type(n: u8) -> Option<ObjectType> {
    match n {
        1 => Some(ObjectType::Commit),
        2 => Some(ObjectType::Tree),
        3 => Some(ObjectType::Blob),
        4 => Some(ObjectType::Tag),
        _ => None,
    }
}
