//! An object store in memory, for the tests that read a bundle back.
//!
//! It is both halves of what [`ObjectStore`] is for: what a bundle's objects
//! are handed to, and what a delta base is read back from when the bundle
//! doesn't carry one — a differential bundle's pack is thin, so its deltas may
//! be written against objects the reader is expected to have already.

use crate::ObjectStore;
use futures::FutureExt;
use futures::future::LocalBoxFuture;
use gib_object::{ObjectId, ObjectType, RawObject};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

#[derive(Default)]
pub(crate) struct MemoryStore {
    objects: RefCell<BTreeMap<ObjectId, Rc<RawObject>>>,
    /// How many objects the reader asked for rather than rebuilding from its
    /// own window — what the fallback path is counted by.
    reads: Cell<usize>,
}

impl MemoryStore {
    /// Put an object in directly, as the objects a thin bundle's deltas expect
    /// the reader to have already arrive: from somewhere other than the bundle.
    pub(crate) fn seed(&self, object_type: ObjectType, body: Vec<u8>) -> ObjectId {
        let raw = Rc::new(RawObject { object_type, body });
        let id = raw.compute_id();
        self.objects.borrow_mut().insert(id, raw);
        id
    }

    pub(crate) fn ids(&self) -> BTreeSet<ObjectId> {
        self.objects.borrow().keys().copied().collect()
    }

    pub(crate) fn body(&self, id: ObjectId) -> Option<Vec<u8>> {
        self.objects.borrow().get(&id).map(|o| o.body.clone())
    }

    pub(crate) fn object_type(&self, id: ObjectId) -> Option<ObjectType> {
        self.objects.borrow().get(&id).map(|o| o.object_type)
    }

    pub(crate) fn len(&self) -> usize {
        self.objects.borrow().len()
    }

    pub(crate) fn reads(&self) -> usize {
        self.reads.get()
    }
}

impl ObjectStore for MemoryStore {
    fn put(&self, id: ObjectId, object: Rc<RawObject>) -> LocalBoxFuture<'_, anyhow::Result<()>> {
        self.objects.borrow_mut().insert(id, object);
        async { Ok(()) }.boxed_local()
    }

    fn get(&self, id: ObjectId) -> LocalBoxFuture<'_, anyhow::Result<Option<RawObject>>> {
        self.reads.set(self.reads.get() + 1);
        let found = self.objects.borrow().get(&id).map(|o| RawObject {
            object_type: o.object_type,
            body: o.body.clone(),
        });
        async move { Ok(found) }.boxed_local()
    }
}
