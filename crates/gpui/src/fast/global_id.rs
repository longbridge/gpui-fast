//! Global element ids that hash their path once, and the per-frame cache that hands the same id out again.
//!
//! Also here: the ids the inspector finds elements by, which are copies of
//! the element id stack too, and so are only built while it is open.

use crate::{ElementId, GlobalElementId};
use collections::FxHashMap;
use std::{mem, sync::Arc};

/// What a [`GlobalElementId`] hashes to: the hash of its path, worked out once.
pub(crate) type PathHash = u64;

/// The global id of the element id stack of `window`, handed out again from
/// the [`GlobalIdCache`] when it was handed out this frame or the last.
#[inline(always)]
pub(crate) fn current(window: &mut crate::Window) -> GlobalElementId {
    window.global_ids.get(&window.element_id_stack)
}

/// A new global id for `path`, not handed out again.
#[cfg(any(feature = "inspector", debug_assertions))]
#[inline(always)]
pub(crate) fn from_path(path: &[ElementId]) -> GlobalElementId {
    GlobalElementId::new(Arc::from(path))
}

/// Element state is looked up by a [`GlobalElementId`] several times per
/// element in every frame, and hashing its path of ids each time, names byte
/// by byte, cost more than the lookups did. The path's hash is therefore worked
/// out once, when the id is made, and is all the id hashes to.
impl GlobalElementId {
    pub(crate) fn new(path: Arc<[ElementId]>) -> Self {
        let hash = Self::path_hash(&path);
        GlobalElementId(path, hash)
    }

    fn path_hash(path: &[ElementId]) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = collections::FxHasher::default();
        path.hash(&mut hasher);
        hasher.finish()
    }
}

impl Default for GlobalElementId {
    fn default() -> Self {
        GlobalElementId::new(Arc::from([]))
    }
}

impl PartialEq for GlobalElementId {
    fn eq(&self, other: &Self) -> bool {
        self.1 == other.1 && self.0 == other.0
    }
}

impl Eq for GlobalElementId {}

impl std::hash::Hash for GlobalElementId {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        state.write_u64(self.1);
    }
}

/// The global ids handed out this frame and the last, by the hash of their
/// path.
///
/// An element whose path is the one it had on the frame before, which is
/// nearly every element, is given the id it had then, rather than a copy of
/// the whole element id stack to be dropped id by id when the frame is done.
#[derive(Default)]
pub(crate) struct GlobalIdCache {
    previous: FxHashMap<u64, GlobalElementId>,
    current: FxHashMap<u64, GlobalElementId>,
}

impl GlobalIdCache {
    /// The global id of `path`: one handed out already if there is one, a
    /// new one otherwise.
    pub(crate) fn get(&mut self, path: &[ElementId]) -> GlobalElementId {
        let hash = GlobalElementId::path_hash(path);
        if let Some(id) = self.current.get(&hash)
            && *id.0 == *path
        {
            return id.clone();
        }
        let id = match self.previous.get(&hash) {
            Some(id) if *id.0 == *path => id.clone(),
            _ => GlobalElementId(Arc::from(path), hash),
        };
        self.current.insert(hash, id.clone());
        id
    }

    /// Keeps this frame's ids for the next one to find, and forgets the
    /// previous frame's.
    pub(crate) fn finish_frame(&mut self) {
        mem::swap(&mut self.previous, &mut self.current);
        self.current.clear();
    }
}

#[cfg(any(feature = "inspector", debug_assertions))]
impl crate::Window {
    /// Whether the inspector is open, so elements need the ids it finds them by.
    pub(crate) fn inspector_enabled(&self) -> bool {
        self.inspector.is_some()
    }
}

/// `element`'s source location, if the inspector is open to find the
/// element by it.
///
/// The path the inspector finds an element by is a copy of the whole
/// element id stack, so it is only built while the inspector is open.
/// Opening it refreshes the window.
#[cfg(any(feature = "inspector", debug_assertions))]
#[inline(always)]
pub(crate) fn inspected(
    window: &crate::Window,
    element: &impl crate::Element,
) -> Option<&'static core::panic::Location<'static>> {
    element
        .source_location()
        .filter(|_| window.inspector_enabled())
}

/// Lets go of the inspector's bookkeeping once it has been closed, so a
/// window without it open holds none.
#[cfg(any(feature = "inspector", debug_assertions))]
#[inline(always)]
pub(crate) fn release_closed_inspector_ids(window: &mut crate::Window) {
    if window.inspector_enabled() {
        return;
    }
    for frame in [&mut window.rendered_frame, &mut window.next_frame] {
        frame.next_inspector_instance_ids = FxHashMap::default();
        frame.inspector_hitboxes = FxHashMap::default();
    }
}

/// Runs `f` with the inspector's state for the element `inspector_id`
/// names, if that is the element the inspector has selected, and does
/// nothing otherwise. [`crate::Window::with_inspector_state`] runs `f` with
/// `None` for every other element, which costs a div a clone of its style on
/// every frame for nothing, so divs call this instead.
#[cfg(any(feature = "inspector", debug_assertions))]
#[inline]
pub(crate) fn with_active_inspector_state<T: 'static, R>(
    window: &mut crate::Window,
    inspector_id: Option<&crate::InspectorElementId>,
    cx: &mut crate::App,
    f: impl FnOnce(&mut Option<T>, &mut crate::Window) -> R,
) -> Option<R> {
    let inspector_id = inspector_id?;
    let inspector = window.inspector.as_ref()?;
    if inspector.read(cx).active_element_id() != Some(inspector_id) {
        return None;
    }
    let inspector = inspector.clone();
    Some(inspector.update(cx, |inspector, _cx| {
        inspector.with_active_element_state(window, f)
    }))
}

#[cfg(test)]
mod tests {
    use super::{ElementId, GlobalElementId, GlobalIdCache};
    use std::hash::{BuildHasher, BuildHasherDefault};
    use std::sync::Arc;

    /// An id's hash is worked out from its path when it is made, so ids made
    /// apart from the same path have to agree, and ids of different paths
    /// must not be taken for one another even where their hashes would meet.
    #[test]
    fn global_ids_compare_and_hash_by_path() {
        let path = |ids: &[&'static str]| {
            GlobalElementId::new(ids.iter().map(|id| ElementId::from(*id)).collect())
        };
        let hash = |id: &GlobalElementId| {
            BuildHasherDefault::<collections::FxHasher>::default().hash_one(id)
        };

        let a = path(&["root", "table", "row"]);
        let b = path(&["root", "table", "row"]);
        assert_eq!(a, b);
        assert_eq!(hash(&a), hash(&b));

        let c = path(&["root", "table", "cell"]);
        assert_ne!(a, c);

        // A path that happens to share another's hash is still a different id.
        let mut forged = c;
        forged.1 = a.1;
        assert_ne!(a, forged);

        assert_eq!(GlobalElementId::default(), path(&[]));
    }

    /// A path asked for again this frame or the next gets the id already
    /// made for it, not a new copy; one unused for a whole frame is let go,
    /// and one whose hash is shared with another path is never mistaken
    /// for it.
    #[test]
    fn global_ids_are_reused_while_their_path_is_in_use() {
        let path = |ids: &[&'static str]| -> Vec<ElementId> {
            ids.iter().map(|id| ElementId::from(*id)).collect()
        };
        let row = path(&["root", "table", "row"]);
        let cell = path(&["root", "table", "cell"]);
        let mut cache = GlobalIdCache::default();

        let first = cache.get(&row);
        assert!(Arc::ptr_eq(&first.0, &cache.get(&row).0));

        cache.finish_frame();
        let next = cache.get(&row);
        assert!(
            Arc::ptr_eq(&first.0, &next.0),
            "a path in use last frame keeps its id"
        );

        cache.finish_frame();
        cache.finish_frame();
        let later = cache.get(&row);
        assert!(
            !Arc::ptr_eq(&first.0, &later.0),
            "a path unused for a frame is let go"
        );
        assert_eq!(first, later);

        let hash = GlobalElementId::path_hash(&row);
        cache
            .current
            .insert(hash, GlobalElementId(Arc::from(&*cell), hash));
        assert_eq!(&*cache.get(&row).0, &*row);
    }
}
