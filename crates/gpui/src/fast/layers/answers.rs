//! Questions about where a scroll container is scrolled to, asked as a view
//! renders, recorded with their answers rather than as reads of the offset.
//!
//! A view asking whether its list is scrolled to its end, whether an item
//! is above or below the viewport, or which child of a scroll handle shows
//! at its top builds what it builds from the answer, not from the offset.
//! The answer is recorded ([`Answer`]) where an offset read would be
//! ([`invalidate::note_offset_read`]); when the view's record is judged,
//! the question is asked again of the state as it is then, and only a
//! changed answer counts as a change. A scroll that leaves every answer as
//! it was neither builds the view again nor keeps its container off its
//! layer.
//!
//! The question is asked again of the live state: the record is judged
//! where the view would otherwise render, which reads the state as it is
//! there. The one exception is the list being prepainted, whose state is
//! borrowed while its layer is decided on: it is asked of the list as it
//! was when its prepaint began ([`ListPrepainting`]), before it lays its
//! rows out. A state that is gone, or borrowed otherwise, answers nothing,
//! which counts as a change.
//!
//! What builds on the offset itself — `ListState::logical_scroll_top`,
//! `bounds_for_item`, `scroll_px_offset_for_scrollbar`, `ScrollHandle::offset`
//! — stays a read of the offset: what a caller does with a pixel value
//! cannot be told.

use std::{
    cell::{Cell, RefCell},
    rc::{Rc, Weak},
};

use crate::fast::dependencies::StateVersion;
use crate::fast::layers::{COMPILED, invalidate};
use crate::{
    Bounds, Count, Edges, ListItem, ListItemSummary, ListOffset, ListState, Pixels, ScrollHandle,
    ScrollHandleState, StateInner, UniformListScrollHandle, point, px,
};
use sum_tree::{Bias, SumTree};

thread_local! {
    /// How many questions are being answered: the reads of the offset made
    /// while answering one are the answer's.
    static ANSWERING: Cell<usize> = const { Cell::new(0) };
    /// The lists being prepainted, by the address of their state's version
    /// counter, as their prepaint began. See [`ListPrepainting`].
    static PREPAINTING: RefCell<Vec<(usize, ListGeometry)>> = const { RefCell::new(Vec::new()) };
    /// The scroll handles asked to scroll to the bottom where they were, with
    /// the version of their state then. See [`note_scroll_to_bottom`].
    #[allow(clippy::type_complexity)]
    static QUIET_SCROLLS_TO_BOTTOM: RefCell<Vec<(Weak<RefCell<ScrollHandleState>>, u64)>> =
        const { RefCell::new(Vec::new()) };
}

/// Whether a question is being answered: reads of the offset are not noted.
#[inline]
pub(crate) fn answering() -> bool {
    ANSWERING.get() > 0
}

/// Notes reads of the offset made while it lives as the answer's, which was
/// recorded before.
pub(crate) struct Answering(bool);

impl Answering {
    fn begin() -> Self {
        if !COMPILED {
            return Answering(false);
        }
        ANSWERING.set(ANSWERING.get() + 1);
        Answering(true)
    }
}

impl Drop for Answering {
    fn drop(&mut self) {
        if self.0 {
            ANSWERING.set(ANSWERING.get() - 1);
        }
    }
}

/// A question asked of a scroll state while drawing.
#[derive(Clone)]
enum Question {
    /// `ListState::is_scrolled_to_end`.
    ListAtEnd(Weak<RefCell<StateInner>>),
    /// `ListState::item_is_above_viewport`.
    ListItemAbove(Weak<RefCell<StateInner>>, usize),
    /// `ListState::item_is_below_viewport`.
    ListItemBelow(Weak<RefCell<StateInner>>, usize),
    /// `ScrollHandle::top_item`.
    TopItem(Weak<RefCell<ScrollHandleState>>),
    /// `ScrollHandle::bottom_item`.
    BottomItem(Weak<RefCell<ScrollHandleState>>),
    /// `UniformListScrollHandle::is_scrolled_to_end`.
    UniformListAtEnd(Weak<RefCell<ScrollHandleState>>),
}

impl Question {
    fn same(&self, other: &Question) -> bool {
        match (self, other) {
            (Question::ListAtEnd(a), Question::ListAtEnd(b)) => a.ptr_eq(b),
            (Question::ListItemAbove(a, i), Question::ListItemAbove(b, j))
            | (Question::ListItemBelow(a, i), Question::ListItemBelow(b, j)) => {
                a.ptr_eq(b) && i == j
            }
            (Question::TopItem(a), Question::TopItem(b))
            | (Question::BottomItem(a), Question::BottomItem(b))
            | (Question::UniformListAtEnd(a), Question::UniformListAtEnd(b)) => a.ptr_eq(b),
            _ => false,
        }
    }

    /// The answer now, if it can be told. `version` counts changes of the
    /// state asked.
    fn ask(&self, version: &StateVersion) -> Option<Value> {
        match self {
            Question::ListAtEnd(list) => Some(Value::Flag(list_geometry(list, version)?.at_end())),
            Question::ListItemAbove(list, ix) => Some(Value::Flag(
                list_geometry(list, version)?.item_is_above_viewport(*ix),
            )),
            Question::ListItemBelow(list, ix) => Some(Value::Flag(
                list_geometry(list, version)?.item_is_below_viewport(*ix),
            )),
            Question::TopItem(handle) => {
                let handle = handle.upgrade()?;
                let handle = handle.try_borrow().ok()?;
                top_item(&handle).map(Value::Index)
            }
            Question::BottomItem(handle) => {
                let handle = handle.upgrade()?;
                let handle = handle.try_borrow().ok()?;
                bottom_item(&handle).map(Value::Index)
            }
            Question::UniformListAtEnd(handle) => {
                let handle = handle.upgrade()?;
                let handle = handle.try_borrow().ok()?;
                uniform_list_at_end(&handle).map(Value::Flag)
            }
        }
    }
}

/// What a question was answered.
#[derive(Clone, Copy, PartialEq)]
enum Value {
    Flag(Option<bool>),
    Index(usize),
}

/// A question asked of a scroll state while drawing, and its answer.
#[derive(Clone)]
pub(crate) struct Answer {
    question: Question,
    value: Value,
}

impl PartialEq for Answer {
    fn eq(&self, other: &Self) -> bool {
        self.question.same(&other.question) && self.value == other.value
    }
}

impl Answer {
    /// Whether `other` asks the same question, whatever its answer.
    pub(crate) fn asks_as(&self, other: &Answer) -> bool {
        self.question.same(&other.question)
    }

    /// Whether the question, asked again now of the state `version` counts
    /// changes of, is answered otherwise, or cannot be answered.
    pub(crate) fn changed(&self, version: &StateVersion) -> bool {
        self.question.ask(version) != Some(self.value)
    }
}

/// Records `question`, asked of the state `version` counts changes of, with
/// its answer, for any recording that is open.
fn note(version: &StateVersion, question: Question) {
    if !COMPILED || answering() || !invalidate::recording_offset_reads() {
        return;
    }
    // Asked as the getter asks it, whose borrow of the state is shared.
    if let Some(value) = question.ask(version) {
        invalidate::note_answer_read(version, Answer { question, value });
    } else {
        invalidate::note_offset_read(version);
    }
}

/// Notes that `ListState::is_scrolled_to_end` was asked of `list`.
pub(crate) fn note_list_at_end(list: &ListState) {
    let version = list.0.borrow().version.clone();
    note(&version, Question::ListAtEnd(Rc::downgrade(&list.0)));
}

/// Notes that `ListState::item_is_above_viewport(ix)` was asked of `list`;
/// the reads of the offset answering it make are the answer's.
pub(crate) fn note_list_item_above(list: &ListState, ix: usize) -> Answering {
    let version = list.0.borrow().version.clone();
    note(
        &version,
        Question::ListItemAbove(Rc::downgrade(&list.0), ix),
    );
    Answering::begin()
}

/// Notes that `ListState::item_is_below_viewport(ix)` was asked of `list`;
/// the reads of the offset answering it make are the answer's.
pub(crate) fn note_list_item_below(list: &ListState, ix: usize) -> Answering {
    let version = list.0.borrow().version.clone();
    note(
        &version,
        Question::ListItemBelow(Rc::downgrade(&list.0), ix),
    );
    Answering::begin()
}

/// Notes that `ScrollHandle::top_item` was asked of `handle`.
pub(crate) fn note_top_item(handle: &ScrollHandle) {
    let version = handle.0.borrow().version.clone();
    note(&version, Question::TopItem(Rc::downgrade(&handle.0)));
}

/// Notes that `ScrollHandle::bottom_item` was asked of `handle`.
pub(crate) fn note_bottom_item(handle: &ScrollHandle) {
    let version = handle.0.borrow().version.clone();
    note(&version, Question::BottomItem(Rc::downgrade(&handle.0)));
}

/// Notes that `UniformListScrollHandle::is_scrolled_to_end` was asked of
/// `handle`; the reads of the offset answering it make are the answer's.
pub(crate) fn note_uniform_list_at_end(handle: &UniformListScrollHandle) -> Answering {
    let base = handle.0.borrow().base_handle.0.clone();
    let version = base.borrow().version.clone();
    note(&version, Question::UniformListAtEnd(Rc::downgrade(&base)));
    Answering::begin()
}

/// Marks the state of `handle` changed if scrolling it to its bottom may move
/// it: not when it is there already, as a view keeping a growing box at its
/// bottom finds it on every render, nor when it was asked to already.
///
/// The request is kept all the same, for content that grew since to be laid
/// out at the bottom, which lays the container out again. A container laid
/// out as it was is not prepainted, and the request would wait for the next
/// frame that prepaints it, a wheel scrolling it away from the bottom, say:
/// it is dropped as the frame ends ([`drop_quiet_scrolls_to_bottom`]), as
/// prepainting the container would have done, leaving it where it is.
pub(crate) fn note_scroll_to_bottom(handle: &ScrollHandle, state: &ScrollHandleState) {
    if state.scroll_to_bottom {
        return;
    }
    let there = state
        .offset
        .try_borrow()
        .is_ok_and(|offset| offset.y <= -state.max_offset.y);
    if there {
        QUIET_SCROLLS_TO_BOTTOM.with_borrow_mut(|quiet| {
            quiet.push((Rc::downgrade(&handle.0), state.version.get()));
        });
    } else {
        state.version.bump();
    }
}

/// Drops the requests to scroll to the bottom made where a scroll handle
/// was already, that no prepaint took since: the handle has not changed
/// since, and is there still. See [`note_scroll_to_bottom`].
pub(crate) fn drop_quiet_scrolls_to_bottom() {
    let quiet = QUIET_SCROLLS_TO_BOTTOM.with_borrow_mut(std::mem::take);
    for (handle, version) in quiet {
        if let Some(handle) = handle.upgrade()
            && let Ok(mut state) = handle.try_borrow_mut()
            && state.version.get() == version
        {
            state.scroll_to_bottom = false;
        }
    }
}

/// What a `list`'s state answers questions about its viewport from.
#[derive(Clone)]
pub(crate) struct ListGeometry {
    bounds: Option<Bounds<Pixels>>,
    padding: Edges<Pixels>,
    items: SumTree<ListItem>,
    scroll_top: ListOffset,
}

impl ListGeometry {
    pub(crate) fn of(state: &StateInner) -> Self {
        ListGeometry {
            bounds: state.last_layout_bounds,
            padding: state.last_padding.unwrap_or_default(),
            items: state.items.clone(),
            scroll_top: state.logical_scroll_top(),
        }
    }

    /// The height of the items before item `ix`.
    fn height_before(&self, ix: usize) -> Pixels {
        let (start, ..) = self
            .items
            .find::<ListItemSummary, _>((), &Count(ix), Bias::Right);
        start.height
    }

    /// As `ListState::is_scrolled_to_end` answers.
    pub(crate) fn at_end(&self) -> Option<bool> {
        let bounds = self.bounds?;
        let summary = self.items.summary();
        if summary.has_unknown_height {
            return None;
        }
        let content_height = summary.height + self.padding.top + self.padding.bottom;
        let scroll_max = (content_height - bounds.size.height).max(px(0.));
        if scroll_max <= px(0.) {
            return None;
        }
        let scroll_top =
            self.height_before(self.scroll_top.item_ix) + self.scroll_top.offset_in_item;
        Some(scroll_top >= scroll_max)
    }

    /// As `ListState::bounds_for_item` answers.
    fn bounds_for_item(&self, ix: usize) -> Option<Bounds<Pixels>> {
        let bounds = self.bounds.unwrap_or_default();
        if ix < self.scroll_top.item_ix {
            return None;
        }
        let scroll_top =
            self.height_before(self.scroll_top.item_ix) + self.scroll_top.offset_in_item;
        let mut cursor = self.items.cursor::<ListItemSummary>(());
        cursor.seek(&Count(ix), Bias::Right);
        if let Some(&ListItem::Measured { size, .. }) = cursor.item()
            && cursor.start().count == ix
        {
            let top = bounds.top() + cursor.start().height - scroll_top;
            return Some(Bounds::from_corners(
                point(bounds.left(), top),
                point(bounds.right(), top + size.height),
            ));
        }
        None
    }

    /// As `ListState::item_is_above_viewport` answers.
    pub(crate) fn item_is_above_viewport(&self, ix: usize) -> Option<bool> {
        let viewport = self.bounds?;
        if ix < self.scroll_top.item_ix {
            return Some(true);
        }
        Some(self.bounds_for_item(ix)?.bottom() <= viewport.top())
    }

    /// As `ListState::item_is_below_viewport` answers.
    pub(crate) fn item_is_below_viewport(&self, ix: usize) -> Option<bool> {
        let viewport = self.bounds?;
        if ix < self.scroll_top.item_ix {
            return Some(false);
        }
        Some(self.bounds_for_item(ix)?.top() >= viewport.bottom())
    }
}

/// The geometry of the list whose state `version` counts changes of: as it
/// is, or, while it is being prepainted, as its prepaint began.
fn list_geometry(list: &Weak<RefCell<StateInner>>, version: &StateVersion) -> Option<ListGeometry> {
    let list = list.upgrade()?;
    match list.try_borrow() {
        Ok(state) => Some(ListGeometry::of(&state)),
        Err(_) => PREPAINTING.with_borrow(|lists| {
            lists
                .iter()
                .rev()
                .find(|(id, _)| *id == version.id())
                .map(|(_, geometry)| geometry.clone())
        }),
    }
}

/// Keeps the geometry of a `list` whose state is borrowed for its prepaint,
/// as the prepaint began, for the questions asked of it while its layer is
/// decided on, while it lives.
pub(crate) struct ListPrepainting(());

impl ListPrepainting {
    pub(crate) fn begin(state: &StateInner) -> Self {
        let geometry = ListGeometry::of(state);
        PREPAINTING.with_borrow_mut(|lists| lists.push((state.version.id(), geometry)));
        ListPrepainting(())
    }
}

impl Drop for ListPrepainting {
    fn drop(&mut self) {
        PREPAINTING.with_borrow_mut(|lists| {
            lists.pop();
        });
    }
}

/// The child whose bounds hold `y` in `child_bounds`, or the nearest, as
/// `ScrollHandle::top_item` and `bottom_item` find it.
fn child_at(child_bounds: &[Bounds<Pixels>], y: Pixels) -> usize {
    match child_bounds.binary_search_by(|bounds| {
        if y < bounds.top() {
            std::cmp::Ordering::Greater
        } else if y > bounds.bottom() {
            std::cmp::Ordering::Less
        } else {
            std::cmp::Ordering::Equal
        }
    }) {
        Ok(ix) => ix,
        Err(ix) => ix.min(child_bounds.len().saturating_sub(1)),
    }
}

/// As `ScrollHandle::top_item` answers, if its offset can be read.
fn top_item(state: &ScrollHandleState) -> Option<usize> {
    let offset = state.offset.try_borrow().ok()?;
    Some(child_at(&state.child_bounds, state.bounds.top() - offset.y))
}

/// As `ScrollHandle::bottom_item` answers, if its offset can be read.
fn bottom_item(state: &ScrollHandleState) -> Option<usize> {
    let offset = state.offset.try_borrow().ok()?;
    Some(child_at(
        &state.child_bounds,
        state.bounds.bottom() - offset.y,
    ))
}

/// As `UniformListScrollHandle::is_scrolled_to_end` answers, if the offset
/// of its scroll handle can be read.
fn uniform_list_at_end(state: &ScrollHandleState) -> Option<Option<bool>> {
    if state.max_offset.y <= px(0.) {
        return Some(None);
    }
    let offset = state.offset.try_borrow().ok()?;
    Some(Some(-offset.y >= state.max_offset.y))
}
