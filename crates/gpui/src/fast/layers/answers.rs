//! Questions about where a scroll container is scrolled to, asked as a view
//! renders, recorded with their answers rather than as reads of the offset.
//!
//! A view asking whether its list is scrolled to its end, whether an item
//! is above or below the viewport, or which child of a scroll handle shows
//! at its top builds what it builds from the answer, not from the offset.
//! The answer is recorded ([`Answer`]) where an offset read would be
//! ([`invalidate::note_offset_read`]); a record keeps the answers it read of
//! each state together ([`AnswerSet`]). When the view's record is judged,
//! the questions are asked again of the state as it is then, and only a
//! changed answer counts as a change. A scroll that leaves every answer as
//! it was neither builds the view again nor keeps its container off its
//! layer.
//!
//! The questions are asked again of the live state: the record is judged
//! where the view would otherwise render, which reads the state as it is
//! there. The one exception is the list being prepainted, whose state is
//! borrowed while its layer is decided on: it is asked of the list as it
//! was when its prepaint began ([`ListPrepainting`]), before it lays its
//! rows out. A state that is gone, or borrowed otherwise, answers nothing,
//! which counts as a change.
//!
//! An outline beside a transcript asks of every turn above the viewport
//! whether it is above it. Those answered so because they come before the
//! row at the top of the list are kept as the last of them only: they stay
//! so while it still comes before the row at the top.
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
use smallvec::SmallVec;
use sum_tree::{Bias, SumTree};

thread_local! {
    /// How many questions are being answered: the reads of the offset made
    /// while answering one are the answer's.
    static ANSWERING: Cell<usize> = const { Cell::new(0) };
    /// How many [`ListTarget`]s live.
    static LIST_TARGETS: Cell<usize> = const { Cell::new(0) };
    /// The lists being prepainted, by the address of their state's version
    /// counter, as their prepaint began. See [`ListPrepainting`].
    static PREPAINTING: RefCell<Vec<(usize, ListSnapshot)>> = const { RefCell::new(Vec::new()) };
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

/// The scroll state a question is asked of.
#[derive(Clone)]
enum Target {
    List(ListTarget),
    Handle(Weak<RefCell<ScrollHandleState>>),
}

/// A `list`'s state a question is asked of, counted while it lives: a list
/// is kept as its prepaint began only while a question asked of any list
/// may be asked again ([`ListPrepainting`]).
struct ListTarget(Weak<RefCell<StateInner>>);

impl ListTarget {
    fn new(list: &ListState) -> Self {
        LIST_TARGETS.set(LIST_TARGETS.get() + 1);
        ListTarget(Rc::downgrade(&list.0))
    }
}

impl Clone for ListTarget {
    fn clone(&self) -> Self {
        LIST_TARGETS.set(LIST_TARGETS.get() + 1);
        ListTarget(self.0.clone())
    }
}

impl Drop for ListTarget {
    fn drop(&mut self) {
        LIST_TARGETS.set(LIST_TARGETS.get() - 1);
    }
}

/// A question asked of a scroll state while drawing.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Question {
    /// `ListState::is_scrolled_to_end`.
    ListAtEnd,
    /// `ListState::item_is_above_viewport`.
    ListItemAbove(usize),
    /// `ListState::item_is_below_viewport`.
    ListItemBelow(usize),
    /// `ScrollHandle::top_item`.
    TopItem,
    /// `ScrollHandle::bottom_item`.
    BottomItem,
    /// `UniformListScrollHandle::is_scrolled_to_end`.
    UniformListAtEnd,
}

/// What a question was answered.
#[derive(Clone, Copy, PartialEq)]
enum Value {
    Flag(Option<bool>),
    Index(usize),
}

impl Question {
    /// The answer of `list` now.
    fn ask_list(self, list: &ListGeometry) -> Option<Value> {
        Some(Value::Flag(match self {
            Question::ListAtEnd => list.at_end(),
            Question::ListItemAbove(ix) => list.item_is_above_viewport(ix),
            Question::ListItemBelow(ix) => list.item_is_below_viewport(ix),
            _ => return None,
        }))
    }

    /// The answer of `handle` now, if its offset can be read.
    fn ask_handle(self, handle: &ScrollHandleState) -> Option<Value> {
        match self {
            Question::TopItem => top_item(handle).map(Value::Index),
            Question::BottomItem => bottom_item(handle).map(Value::Index),
            Question::UniformListAtEnd => uniform_list_at_end(handle).map(Value::Flag),
            _ => None,
        }
    }
}

/// A question asked of a scroll state while drawing, and its answer, as
/// the log of what is read holds it.
#[derive(Clone)]
pub(crate) struct Answer {
    target: Target,
    question: Question,
    value: Value,
    /// Whether it asked whether an item before the row at the top of the
    /// list is above the viewport, which it is.
    before_top: bool,
}

impl Answer {
    /// Takes the question whether item `other_ix`, before the row at the top
    /// of the list, is above the viewport, asked of the same state, in, if
    /// this one asked the same of another such item: the last of them
    /// stands for all. An outline asks it of every turn above the viewport.
    pub(crate) fn absorb_above_before_top(&mut self, other_ix: usize) -> bool {
        match &mut self.question {
            Question::ListItemAbove(ix) if self.before_top => {
                *ix = (*ix).max(other_ix);
                true
            }
            _ => false,
        }
    }
}

/// The questions a record asked of one scroll state, and their answers.
#[derive(Clone)]
pub(crate) struct AnswerSet {
    target: Target,
    /// By question, each once.
    answers: Answers,
    /// The last item asked whether it is above the viewport, and answered
    /// so for coming before the row at the top of the list.
    above_before_top: Option<usize>,
}

/// Questions and their answers, most often one, which takes no allocation.
#[derive(Clone)]
enum Answers {
    None,
    One((Question, Value)),
    Many(Rc<[(Question, Value)]>),
}

impl Answers {
    fn as_slice(&self) -> &[(Question, Value)] {
        match self {
            Answers::None => &[],
            Answers::One(answer) => std::slice::from_ref(answer),
            Answers::Many(answers) => answers,
        }
    }

    /// Whether both are the same answers, as far as it can be told cheaply.
    fn same(&self, other: &Answers) -> bool {
        match (self, other) {
            (Answers::None, Answers::None) => true,
            (Answers::One(a), Answers::One(b)) => a == b,
            (Answers::Many(a), Answers::Many(b)) => Rc::ptr_eq(a, b),
            _ => false,
        }
    }
}

/// Gathers the answers a record read of one scroll state into an
/// [`AnswerSet`].
pub(crate) struct Gather {
    target: Target,
    answers: SmallVec<[(Question, Value); 4]>,
    above_before_top: Option<usize>,
    /// The one set gathered, while nothing else was.
    only: Option<AnswerSet>,
    gathered: usize,
}

impl Gather {
    fn new(target: &Target) -> Self {
        Gather {
            target: target.clone(),
            answers: SmallVec::new(),
            above_before_top: None,
            only: None,
            gathered: 0,
        }
    }

    pub(crate) fn of_answer(answer: &Answer) -> Self {
        let mut gather = Gather::new(&answer.target);
        gather.add_answer(answer);
        gather
    }

    pub(crate) fn of_set(set: &AnswerSet) -> Self {
        let mut gather = Gather::new(&set.target);
        gather.add_set(set);
        gather
    }

    pub(crate) fn add_answer(&mut self, answer: &Answer) {
        self.only = None;
        self.gathered += 1;
        if answer.before_top {
            if let Question::ListItemAbove(ix) = answer.question {
                self.above_before_top = self.above_before_top.max(Some(ix));
            }
        } else {
            self.answers.push((answer.question, answer.value));
        }
    }

    pub(crate) fn add_set(&mut self, set: &AnswerSet) {
        if self.gathered == 0 {
            self.only = Some(set.clone());
        } else if self.only.as_ref().is_some_and(|only| {
            only.answers.same(&set.answers) && only.above_before_top == set.above_before_top
        }) {
            return;
        } else {
            self.only = None;
        }
        self.gathered += 1;
        self.answers.extend_from_slice(set.answers.as_slice());
        self.above_before_top = self.above_before_top.max(set.above_before_top);
    }

    /// The set gathered, or `None` if a question was answered two ways.
    pub(crate) fn finish(mut self) -> Option<AnswerSet> {
        if let Some(only) = self.only {
            return Some(only);
        }
        let mut unique = self.answers;
        unique.sort_unstable_by_key(|(question, _)| *question);
        if unique
            .windows(2)
            .any(|pair| pair[0].0 == pair[1].0 && pair[0].1 != pair[1].1)
        {
            return None;
        }
        unique.dedup_by_key(|(question, _)| *question);
        // An item before the top is above the viewport.
        if let Some(before) = self.above_before_top
            && unique.iter().any(|(question, value)| {
                matches!(question, Question::ListItemAbove(ix) if *ix <= before)
                    && *value != Value::Flag(Some(true))
            })
        {
            return None;
        }
        let answers = match unique.len() {
            0 => Answers::None,
            1 => Answers::One(unique[0]),
            _ => Answers::Many(unique.as_slice().into()),
        };
        Some(AnswerSet {
            target: self.target,
            answers,
            above_before_top: self.above_before_top,
        })
    }
}

impl AnswerSet {
    /// Whether a question, asked again now of the state `version` counts
    /// changes of, is answered otherwise, or cannot be answered.
    pub(crate) fn changed(&self, version: &StateVersion) -> bool {
        let answers = self.answers.as_slice();
        match &self.target {
            Target::List(list) => with_list_geometry(&list.0, version, |list| {
                if let Some(before) = self.above_before_top
                    && (list.bounds.is_none() || before >= list.scroll_top.item_ix)
                {
                    return true;
                }
                answers
                    .iter()
                    .any(|(question, value)| question.ask_list(list) != Some(*value))
            })
            .unwrap_or(true),
            Target::Handle(handle) => {
                let Some(handle) = handle.upgrade() else {
                    return true;
                };
                let Ok(handle) = handle.try_borrow() else {
                    return true;
                };
                answers
                    .iter()
                    .any(|(question, value)| question.ask_handle(&handle) != Some(*value))
            }
        }
    }
}

/// Records `question`, asked of `list`, with its answer, for any recording
/// that is open.
fn note_list(list: &ListState, question: Question) {
    if !COMPILED || answering() {
        return;
    }
    // Asked as the getter asks it, whose borrow of the state is shared.
    let Ok(state) = list.0.try_borrow() else {
        return;
    };
    let before_top = matches!(question, Question::ListItemAbove(ix)
        if state.last_layout_bounds.is_some() && ix < state.logical_scroll_top().item_ix);
    if let (true, Question::ListItemAbove(ix)) = (before_top, question)
        && invalidate::merge_answer_read(&state.version, |last| last.absorb_above_before_top(ix))
    {
        return;
    }
    if !invalidate::recording_offset_reads() {
        return;
    }
    let geometry = ListGeometry::of(&state);
    let Some(value) = question.ask_list(&geometry) else {
        invalidate::note_offset_read(&state.version);
        return;
    };
    invalidate::note_answer_read(
        &state.version,
        Answer {
            target: Target::List(ListTarget::new(list)),
            question,
            value,
            before_top,
        },
    );
}

/// Records `question`, asked of the scroll handle state `handle`, with its
/// answer, for any recording that is open.
fn note_handle(handle: &Rc<RefCell<ScrollHandleState>>, question: Question) {
    if !COMPILED || answering() || !invalidate::recording_offset_reads() {
        return;
    }
    let Ok(state) = handle.try_borrow() else {
        return;
    };
    match question.ask_handle(&state) {
        Some(value) => invalidate::note_answer_read(
            &state.version,
            Answer {
                target: Target::Handle(Rc::downgrade(handle)),
                question,
                value,
                before_top: false,
            },
        ),
        None => invalidate::note_offset_read(&state.version),
    }
}

/// Notes that `ListState::is_scrolled_to_end` was asked of `list`.
pub(crate) fn note_list_at_end(list: &ListState) {
    note_list(list, Question::ListAtEnd);
}

/// Notes that `ListState::item_is_above_viewport(ix)` was asked of `list`;
/// the reads of the offset answering it make are the answer's.
pub(crate) fn note_list_item_above(list: &ListState, ix: usize) -> Answering {
    note_list(list, Question::ListItemAbove(ix));
    Answering::begin()
}

/// Notes that `ListState::item_is_below_viewport(ix)` was asked of `list`;
/// the reads of the offset answering it make are the answer's.
pub(crate) fn note_list_item_below(list: &ListState, ix: usize) -> Answering {
    note_list(list, Question::ListItemBelow(ix));
    Answering::begin()
}

/// Notes that `ScrollHandle::top_item` was asked of `handle`.
pub(crate) fn note_top_item(handle: &ScrollHandle) {
    note_handle(&handle.0, Question::TopItem);
}

/// Notes that `ScrollHandle::bottom_item` was asked of `handle`.
pub(crate) fn note_bottom_item(handle: &ScrollHandle) {
    note_handle(&handle.0, Question::BottomItem);
}

/// Notes that `UniformListScrollHandle::is_scrolled_to_end` was asked of
/// `handle`; the reads of the offset answering it make are the answer's.
pub(crate) fn note_uniform_list_at_end(handle: &UniformListScrollHandle) -> Answering {
    if COMPILED && !answering() && invalidate::recording_offset_reads() {
        let base = handle.0.borrow().base_handle.0.clone();
        note_handle(&base, Question::UniformListAtEnd);
    }
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
    if QUIET_SCROLLS_TO_BOTTOM.with_borrow(|quiet| quiet.is_empty()) {
        return;
    }
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
pub(crate) struct ListGeometry<'a> {
    bounds: Option<Bounds<Pixels>>,
    padding: Edges<Pixels>,
    items: &'a SumTree<ListItem>,
    scroll_top: ListOffset,
}

impl<'a> ListGeometry<'a> {
    pub(crate) fn of(state: &'a StateInner) -> Self {
        ListGeometry {
            bounds: state.last_layout_bounds,
            padding: state.last_padding.unwrap_or_default(),
            items: &state.items,
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

    /// How far the list is scrolled, in pixels.
    fn scroll_top_px(&self) -> Pixels {
        self.height_before(self.scroll_top.item_ix) + self.scroll_top.offset_in_item
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
        Some(self.scroll_top_px() >= scroll_max)
    }

    /// As `ListState::bounds_for_item` answers.
    fn bounds_for_item(&self, ix: usize) -> Option<Bounds<Pixels>> {
        let bounds = self.bounds.unwrap_or_default();
        if ix < self.scroll_top.item_ix {
            return None;
        }
        let scroll_top = self.scroll_top_px();
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

/// A `list`'s geometry, as its prepaint began.
struct ListSnapshot {
    bounds: Option<Bounds<Pixels>>,
    padding: Edges<Pixels>,
    items: SumTree<ListItem>,
    scroll_top: ListOffset,
}

impl ListSnapshot {
    fn geometry(&self) -> ListGeometry<'_> {
        ListGeometry {
            bounds: self.bounds,
            padding: self.padding,
            items: &self.items,
            scroll_top: self.scroll_top,
        }
    }
}

/// `f` of the geometry of the list whose state `version` counts changes of:
/// as it is, or, while it is being prepainted, as its prepaint began.
fn with_list_geometry<R>(
    list: &Weak<RefCell<StateInner>>,
    version: &StateVersion,
    f: impl FnOnce(&ListGeometry) -> R,
) -> Option<R> {
    let list = list.upgrade()?;
    match list.try_borrow() {
        Ok(state) => Some(f(&ListGeometry::of(&state))),
        Err(_) => PREPAINTING.with_borrow(|lists| {
            lists
                .iter()
                .rev()
                .find(|(id, _)| *id == version.id())
                .map(|(_, snapshot)| f(&snapshot.geometry()))
        }),
    }
}

/// Keeps the geometry of a `list` whose state is borrowed for its prepaint,
/// as the prepaint began, for the questions asked of it while its layer is
/// decided on, while it lives.
pub(crate) struct ListPrepainting(bool);

impl ListPrepainting {
    pub(crate) fn begin(state: &StateInner) -> Self {
        if LIST_TARGETS.get() == 0 {
            return ListPrepainting(false);
        }
        let snapshot = ListSnapshot {
            bounds: state.last_layout_bounds,
            padding: state.last_padding.unwrap_or_default(),
            items: state.items.clone(),
            scroll_top: state.logical_scroll_top(),
        };
        PREPAINTING.with_borrow_mut(|lists| lists.push((state.version.id(), snapshot)));
        ListPrepainting(true)
    }
}

impl Drop for ListPrepainting {
    fn drop(&mut self) {
        if self.0 {
            PREPAINTING.with_borrow_mut(|lists| {
                lists.pop();
            });
        }
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
