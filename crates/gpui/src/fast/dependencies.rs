//! What a retained subtree read while it was built — entities, globals and versioned state — and how the app records it.

use std::{
    any::TypeId,
    cell::{Cell, RefCell},
    ops::Range,
    rc::Rc,
};

use collections::{FxHashMap, FxHashSet, TypeIdHashMap};

use crate::{App, EntityId, EntityMap, ListOffset};

/// The app's side of recording what retained subtrees read: when each global
/// last changed, and what was read while a recording is open.
#[derive(Default)]
pub(crate) struct AppDependencies {
    /// Counts the changes to globals, each of which is stamped into
    /// `global_changed_at`, so a retained subtree can tell whether a global
    /// it read has changed since it was built.
    global_generation: u64,
    global_changed_at: TypeIdHashMap<u64>,
    /// Every global read while a recording is open. See
    /// [`App::begin_recording_dependencies`].
    global_read_log: RefCell<Vec<TypeId>>,
    /// Every [`StateVersion`] read while a recording is open, with the
    /// version it was at.
    state_read_log: RefCell<Vec<(StateVersion, u64)>>,
    /// For each open recording, innermost last, the stretches of the logs that
    /// recordings nested in it, or dependencies replayed into it, took up:
    /// what it read through a nested subtree rather than itself.
    nested: Vec<Vec<LogRanges>>,
}

/// Stretches of the three read logs.
#[derive(Clone)]
struct LogRanges {
    entities: Range<usize>,
    globals: Range<usize>,
    states: Range<usize>,
}

impl AppDependencies {
    /// Stamps a change to the global of type `global_type`.
    pub(crate) fn global_changed(&mut self, global_type: TypeId) {
        // Stamped on every change, not only the first one an effect is queued
        // for: a subtree built in between has seen only the first.
        self.global_generation += 1;
        self.global_changed_at
            .insert(global_type, self.global_generation);
    }
}

impl App {
    /// Starts recording what is read from here on — the entities accessed and
    /// the globals read — for a subtree that is drawn again from what it drew
    /// while none of it changes. Recordings nest; each sees everything read
    /// while it is open, including what nested ones saw.
    pub(crate) fn begin_recording_dependencies(&mut self) -> DependencyRecording {
        self.dependencies.nested.push(Vec::new());
        DependencyRecording {
            entities: self.entities.begin_recording(),
            globals: self.dependencies.global_read_log.get_mut().len(),
            states: self.dependencies.state_read_log.get_mut().len(),
            generation: self.dependencies.global_generation,
            updates: self.entities.access_log.update_generation,
        }
    }

    /// Ends `recording`, returning what was read while it was open: all of
    /// it, and what was read outside the recordings nested in it.
    pub(crate) fn finish_recording_dependencies(
        &mut self,
        recording: DependencyRecording,
    ) -> RecordedDependencies {
        let log = &mut self.dependencies;
        let nested = log.nested.pop().unwrap_or_default();
        let ranges = LogRanges {
            entities: recording.entities..self.entities.access_log.len(),
            globals: recording.globals..log.global_read_log.get_mut().len(),
            states: recording.states..log.state_read_log.get_mut().len(),
        };
        let globals_log = log.global_read_log.get_mut();
        let states_log = log.state_read_log.get_mut();
        let own_globals = outside(
            globals_log,
            &ranges.globals,
            nested.iter().map(|n| &n.globals),
        );
        let own_states = outside(states_log, &ranges.states, nested.iter().map(|n| &n.states));
        let mut globals = globals_log[recording.globals..].to_vec();
        let states = dedup_states(&states_log[recording.states..]);
        let own_entities = outside(
            &self.entities.access_log.access_log.borrow(),
            &ranges.entities,
            nested.iter().map(|n| &n.entities),
        );
        let entities = self.entities.finish_recording(recording.entities);
        if let Some(parent) = self.dependencies.nested.last_mut() {
            parent.push(ranges);
        }
        if !self.entities.is_recording() {
            self.dependencies.global_read_log.get_mut().clear();
            self.dependencies.state_read_log.get_mut().clear();
        }
        globals.sort_unstable();
        globals.dedup();
        RecordedDependencies {
            all: RenderDependencies {
                entities: entities.into(),
                globals: globals.into(),
                states,
                // As of when the recording began, so that a global written
                // while it was open, after being read, counts as changed.
                generation: recording.generation,
                updates: recording.updates,
            },
            own: RenderDependencies::from_reads(own_entities, own_globals, &own_states, &recording),
        }
    }

    /// Tells the window, and any recording that is open, that `dependencies`
    /// were read again, as they are when a subtree built from them is reused.
    pub(crate) fn replay_dependencies(&mut self, dependencies: &RenderDependencies) {
        let start = LogRanges {
            entities: self.entities.access_log.len()..0,
            globals: self.dependencies.global_read_log.get_mut().len()..0,
            states: self.dependencies.state_read_log.get_mut().len()..0,
        };
        self.entities.extend_accessed(dependencies.entities.iter());
        if self.entities.is_recording() {
            self.dependencies
                .global_read_log
                .get_mut()
                .extend(dependencies.globals.iter().copied());
            self.dependencies
                .state_read_log
                .get_mut()
                .extend(dependencies.states.iter().cloned());
            // Read through the subtree being reused, not by the recording
            // it is reused in.
            let ranges = LogRanges {
                entities: start.entities.start..self.entities.access_log.len(),
                globals: start.globals.start..self.dependencies.global_read_log.get_mut().len(),
                states: start.states.start..self.dependencies.state_read_log.get_mut().len(),
            };
            if let Some(open) = self.dependencies.nested.last_mut() {
                open.push(ranges);
            }
        }
    }

    /// Records, for any recording that is open, that the state `version`
    /// belongs to was read as it is now.
    #[inline]
    pub(crate) fn note_state_read(&self, version: &StateVersion) {
        if self.entities.is_recording() {
            self.dependencies
                .state_read_log
                .borrow_mut()
                .push((version.clone(), version.get()));
        }
    }

    /// Whether anything in `dependencies` may have changed since they were
    /// recorded: one of the entities was updated, or notified while drawing,
    /// since, or one of the globals has been written.
    ///
    /// An entity notified without being updated — as a scroll wheel, a
    /// dragged scrollbar or an animation notifies the view to draw again —
    /// holds what it held: the view notified is built again, but a view that
    /// read it is not. What scrolled is tracked by the scroll state's own
    /// version.
    pub(crate) fn dependencies_changed(&self, dependencies: &RenderDependencies) -> bool {
        self.entities
            .access_log
            .updated_since(&dependencies.entities, dependencies.updates)
            || dependencies.globals.iter().any(|global| {
                self.dependencies
                    .global_changed_at
                    .get(global)
                    .is_some_and(|changed_at| *changed_at > dependencies.generation)
            })
            || dependencies
                .states
                .iter()
                .any(|(version, read_at)| version.get() != *read_at)
    }

    /// Records, for any recording that is open, that the global of type
    /// `global` was read.
    #[inline]
    pub(crate) fn note_global_read(&self, global: TypeId) {
        if self.entities.is_recording() {
            self.dependencies.global_read_log.borrow_mut().push(global);
        }
    }
}

/// The entity map's side of recording what retained subtrees read.
#[derive(Default)]
pub(crate) struct EntityAccessLog {
    /// Every entity accessed while a recording is open, in order and with
    /// repeats, for a retained subtree to learn what it was built from. See
    /// [`App::begin_recording_dependencies`].
    access_log: RefCell<Vec<EntityId>>,
    /// How many recordings are open.
    recordings: Cell<usize>,
    /// Counts the entities updated while no recording is open, each of which
    /// is stamped into `updated_at`.
    update_generation: u64,
    /// When each entity was last updated while no recording was open. See
    /// [`EntityMap::note_update`].
    updated_at: FxHashMap<EntityId, u64>,
}

impl EntityAccessLog {
    /// How many accesses the log holds.
    fn len(&self) -> usize {
        self.access_log.borrow().len()
    }

    /// Whether any of `entities` was updated after `generation`.
    fn updated_since(&self, entities: &[EntityId], generation: u64) -> bool {
        generation != self.update_generation
            && entities.iter().any(|entity| {
                self.updated_at
                    .get(entity)
                    .is_some_and(|updated_at| *updated_at > generation)
            })
    }

    /// Forgets when a released entity was updated.
    pub(crate) fn forget(&mut self, entity_id: EntityId) {
        self.updated_at.remove(&entity_id);
    }
}

impl EntityMap {
    /// Records, for any recording that is open, that `entity_id` was
    /// accessed.
    #[inline]
    pub(crate) fn note_access(&self, entity_id: EntityId) {
        if self.access_log.recordings.get() > 0 {
            self.access_log.access_log.borrow_mut().push(entity_id);
        }
    }

    /// Records that `entity_id` is notified. A notification while a subtree
    /// is being drawn — a view changing a model it read as it renders — counts
    /// as an update: nothing else tells whether it changed what the model
    /// holds. One outside drawing that follows an update was counted by the
    /// update; one alone changes nothing a view could have read.
    #[inline]
    pub(crate) fn note_notify(&mut self, entity_id: EntityId) {
        let log = &mut self.access_log;
        if log.recordings.get() > 0 {
            log.update_generation += 1;
            log.updated_at.insert(entity_id, log.update_generation);
        }
    }

    /// Records that `entity_id` is being updated, as [`Self::note_access`]
    /// does for an access.
    ///
    /// An entity updated outside of drawing — by a task, a listener, an
    /// action — may have changed without being notified, as when a view
    /// changes a model it renders and notifies only itself. A retained subtree
    /// that read it is built again, as upstream builds every view under a
    /// notified one again. Updates while a subtree is being built, a view
    /// rendering itself for one, are part of drawing it and are not stamped.
    #[inline]
    pub(crate) fn note_update(&mut self, entity_id: EntityId) {
        self.note_access(entity_id);
        let log = &mut self.access_log;
        if log.recordings.get() == 0 {
            log.update_generation += 1;
            log.updated_at.insert(entity_id, log.update_generation);
        }
    }

    pub fn extend_accessed<'a>(&mut self, entities: impl IntoIterator<Item = &'a EntityId>) {
        let accessed_entities = self.accessed_entities.get_mut();
        let recording = self.access_log.recordings.get() > 0;
        for entity_id in entities {
            accessed_entities.insert(*entity_id);
            if recording {
                self.access_log.access_log.get_mut().push(*entity_id);
            }
        }
    }

    /// Whether any recording is open.
    #[inline]
    pub(crate) fn is_recording(&self) -> bool {
        self.access_log.recordings.get() > 0
    }

    /// Opens a recording, returning where in the access log it starts.
    pub(crate) fn begin_recording(&mut self) -> usize {
        let log = &mut self.access_log;
        log.recordings.set(log.recordings.get() + 1);
        log.access_log.get_mut().len()
    }

    /// Closes the recording that started at `start`, returning the entities it
    /// saw, sorted and without repeats.
    pub(crate) fn finish_recording(&mut self, start: usize) -> Vec<EntityId> {
        let EntityAccessLog {
            access_log,
            recordings,
            ..
        } = &mut self.access_log;
        let log = access_log.get_mut();
        let mut entities = log[start..].to_vec();
        let open = recordings.get() - 1;
        recordings.set(open);
        if open == 0 {
            log.clear();
        }
        entities.sort_unstable();
        entities.dedup();
        entities
    }
}

/// Where a recording started by [`App::begin_recording_dependencies`] begins.
#[derive(Clone, Copy)]
pub(crate) struct DependencyRecording {
    entities: usize,
    globals: usize,
    states: usize,
    generation: u64,
    updates: u64,
}

/// What a recording saw: everything read while it was open, and what was read
/// outside the recordings nested in it, by the subtree itself.
pub(crate) struct RecordedDependencies {
    pub(crate) all: RenderDependencies,
    pub(crate) own: RenderDependencies,
}

/// The entries of `log` in `range` that fall outside every range in `nested`,
/// which lie within `range`, in order.
fn outside<'a, T: Clone>(
    log: &[T],
    range: &Range<usize>,
    nested: impl Iterator<Item = &'a Range<usize>>,
) -> Vec<T> {
    let mut own = Vec::new();
    let mut cursor = range.start;
    for nested in nested {
        if nested.start > cursor {
            own.extend_from_slice(&log[cursor..nested.start]);
        }
        cursor = cursor.max(nested.end);
    }
    if range.end > cursor {
        own.extend_from_slice(&log[cursor..range.end]);
    }
    own
}

/// What a retained subtree read while it was built: the entities it accessed
/// and the globals it read, as of a global generation. While none of them has
/// changed, building the subtree again would build the same thing.
#[derive(Clone, Default)]
pub(crate) struct RenderDependencies {
    pub(crate) entities: Rc<[EntityId]>,
    pub(crate) globals: Rc<[TypeId]>,
    /// Element state kept outside of entities — scroll handles, list states
    /// — with the version each was read at.
    pub(crate) states: Rc<[(StateVersion, u64)]>,
    pub(crate) generation: u64,
    /// The entity update generation the recording began at. See
    /// [`EntityMap::note_update`].
    pub(crate) updates: u64,
}

/// A counter that state shared outside of entities — a scroll handle, a list
/// state — increments whenever it changes, so that a retained subtree that
/// read it is built again, as it would be for an entity that was notified.
#[derive(Clone, Default, Debug)]
pub(crate) struct StateVersion(Rc<Cell<u64>>);

impl StateVersion {
    pub(crate) fn get(&self) -> u64 {
        self.0.get()
    }

    /// Marks the state as changed.
    pub(crate) fn bump(&self) {
        self.0.set(self.0.get().wrapping_add(1));
    }

    /// Marks the state as changed if `changed`, for a change that may leave
    /// it as it was.
    #[inline]
    pub(crate) fn bump_if(&self, changed: bool) {
        if changed {
            self.bump();
        }
    }

    fn ptr(&self) -> *const Cell<u64> {
        Rc::as_ptr(&self.0)
    }
}

impl ListOffset {
    /// Whether scrolling a list scrolled to `current`, with `pending_scroll`,
    /// to this offset changes where it is scrolled to.
    ///
    /// Scrolling to where it already is, as a view that scrolls its list
    /// while rendering does every frame, changes nothing.
    pub(crate) fn moves_from<P>(
        &self,
        current: Option<ListOffset>,
        pending_scroll: &Option<P>,
    ) -> bool {
        let unchanged = current.is_some_and(|current| {
            current.item_ix == self.item_ix && current.offset_in_item == self.offset_in_item
        }) && pending_scroll.is_none();
        !unchanged
    }
}

/// `states` once each, at the earliest version read, so that a change in
/// between still counts.
fn dedup_states(states: &[(StateVersion, u64)]) -> Rc<[(StateVersion, u64)]> {
    if states.is_empty() {
        return Rc::new([]);
    }
    let mut seen = FxHashSet::default();
    states
        .iter()
        .filter(|(version, _)| seen.insert(version.ptr()))
        .cloned()
        .collect()
}

impl RenderDependencies {
    /// Dependencies on what was read, as of when `recording` began.
    fn from_reads(
        mut entities: Vec<EntityId>,
        mut globals: Vec<TypeId>,
        states: &[(StateVersion, u64)],
        recording: &DependencyRecording,
    ) -> Self {
        entities.sort_unstable();
        entities.dedup();
        globals.sort_unstable();
        globals.dedup();
        Self {
            entities: entities.into(),
            globals: globals.into(),
            states: dedup_states(states),
            generation: recording.generation,
            updates: recording.updates,
        }
    }

    /// Both sets of dependencies at once, as of the earlier generation, so
    /// that a change either would have seen is still seen.
    pub(crate) fn union(&self, other: &Self) -> Self {
        if other.entities.is_empty() && other.globals.is_empty() && other.states.is_empty() {
            return self.clone();
        }
        let mut entities = self.entities.to_vec();
        entities.extend_from_slice(&other.entities);
        entities.sort_unstable();
        entities.dedup();
        let mut globals = self.globals.to_vec();
        globals.extend_from_slice(&other.globals);
        globals.sort_unstable();
        globals.dedup();
        let mut states = self.states.to_vec();
        states.extend_from_slice(&other.states);
        Self {
            entities: entities.into(),
            globals: globals.into(),
            states: dedup_states(&states),
            generation: self.generation.min(other.generation),
            updates: self.updates.min(other.updates),
        }
    }
}
