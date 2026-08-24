//! Reading and writing the JS objects a cached record is stored as.
//!
//! IndexedDB holds plain JS values, so every field crosses the boundary as an
//! `ArrayBuffer` or a number; these are the conversions both directions.

use gib::commit_graph::bloom::BloomSettings;
use gib::object::{ObjectId, ObjectType};
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
