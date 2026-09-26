//! An element that reuses what it drew last frame while its key is unchanged.

use crate::{
    AnyElement, App, Bounds, ContentMask, Element, ElementId, EntityId, GlobalElementId, HitboxId,
    InspectorElementId, IntoElement, LayoutId, PaintIndex, Pixels, PrepaintStateIndex, Style,
    StyleRefinement, Styled, TextStyle, Window,
};
use collections::FxHashSet;
use refineable::Refineable;
use std::{
    any::Any,
    hash::{Hash, Hasher},
    mem,
    ops::Range,
};

/// A subtree that is built only when `key` differs from the one it was drawn
/// with last frame, and otherwise drawn again from what it drew then, without
/// calling `build`, laying it out, prepainting or painting it.
///
/// `key` has to stand for everything the subtree's look and behaviour depend
/// on: two frames with equal keys must build the same subtree. Nothing checks
/// that the application kept this promise, the way [`crate::Context::notify`]
/// is a promise for a view: state the subtree reads that is left out of the key
/// is shown as it was the last time the key changed. The framework's own state
/// is taken care of: a memo is also built again when its bounds, content mask
/// or text style change, when the window is refreshed, and when the pointer
/// moving over it or a scroll inside it changes how it looks.
///
/// A memo is laid out by its own style, not by its content, as
/// [`crate::Entity::cached`] is: give it the size its content takes.
///
/// [`Version`] and [`ContentHash`] are keys ready to use, and [`AnyMemoKey`]
/// holds a key of any type for an API that cannot name it.
pub fn memo<K, E>(
    id: impl Into<ElementId>,
    key: K,
    build: impl FnOnce(&mut Window, &mut App) -> E + 'static,
) -> Memo<K>
where
    K: PartialEq + 'static,
    E: IntoElement,
{
    Memo {
        id: id.into(),
        key: Some(key),
        style: StyleRefinement::default(),
        build: Some(Box::new(move |window, cx| {
            build(window, cx).into_any_element()
        })),
    }
}

/// An element returned by [`memo`].
pub struct Memo<K> {
    id: ElementId,
    key: Option<K>,
    style: StyleRefinement,
    build: Option<Box<dyn FnOnce(&mut Window, &mut App) -> AnyElement>>,
}

/// A memo key that is a counter its owner increments whenever what it keys
/// changes: the cheapest key there is, for data changed in few places.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Version(pub u64);

/// A memo key made from hashing what the subtree depends on, for data changed
/// in too many places to keep a [`Version`] of. Two different inputs can hash
/// alike, if very rarely; key a memo by the inputs themselves where that must
/// not happen.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ContentHash(u64);

impl ContentHash {
    /// The hash of `value`.
    pub fn of(value: &impl Hash) -> Self {
        let mut hasher = collections::FxHasher::default();
        value.hash(&mut hasher);
        ContentHash(hasher.finish())
    }
}

/// A memo key of any type, for an interface that cannot be generic over the
/// key — a trait whose implementors pick their own. Two keys are equal when
/// they are of the same type and equal as that type.
pub struct AnyMemoKey(Box<dyn DynMemoKey>);

impl AnyMemoKey {
    /// Wraps `key`.
    pub fn new(key: impl PartialEq + 'static) -> Self {
        AnyMemoKey(Box::new(key))
    }
}

impl PartialEq for AnyMemoKey {
    fn eq(&self, other: &Self) -> bool {
        self.0.eq_dyn(other.0.as_any())
    }
}

trait DynMemoKey {
    fn as_any(&self) -> &dyn Any;
    fn eq_dyn(&self, other: &dyn Any) -> bool;
}

impl<K: PartialEq + 'static> DynMemoKey for K {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn eq_dyn(&self, other: &dyn Any) -> bool {
        other.downcast_ref::<K>() == Some(self)
    }
}

/// What a memo remembers from the frame it was last drawn in.
struct MemoState<K> {
    key: K,
    bounds: Bounds<Pixels>,
    content_mask: ContentMask<Pixels>,
    text_style: TextStyle,
    prepaint_range: Range<PrepaintStateIndex>,
    paint_range: Range<PaintIndex>,
    accessed_entities: FxHashSet<EntityId>,
    /// Whether each hitbox whose hover the subtree was painted by was hovered
    /// then. The subtree looks different once any of them is hovered
    /// differently, so it is built again.
    hover_dependencies: Vec<(HitboxId, bool)>,
    /// The keys of the layout nodes the subtree was laid out with, kept alive
    /// while it is reused so that building it again reuses them.
    layout_keys: Vec<u64>,
}

impl<K> MemoState<K> {
    /// Whether every hover the subtree was painted by is still as it was.
    fn hovers_unchanged(&self, window: &Window) -> bool {
        let touch = window.last_input_was_touch();
        self.hover_dependencies
            .iter()
            .all(|(hitbox, hovered)| (!touch && hitbox.is_hovered(window)) == *hovered)
    }
}

impl<K: PartialEq + 'static> Styled for Memo<K> {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl<K: PartialEq + 'static> IntoElement for Memo<K> {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl<K: PartialEq + 'static> Element for Memo<K> {
    type RequestLayoutState = ();
    /// The subtree, when it was built this frame rather than reused.
    type PrepaintState = Option<AnyElement>;

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.refine(&self.style);
        (window.request_layout(style, None, cx), ())
    }

    fn prepaint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let global_id = global_id.expect("a memo always has an id");
        let key = self.key.take().expect("a memo is prepainted once");
        window.with_element_state::<MemoState<K>, _>(global_id, |state, window| {
            let content_mask = window.content_mask();
            let text_style = window.text_style();

            if let Some(mut state) = state
                && state.key == key
                && state.bounds == bounds
                && state.content_mask == content_mask
                && state.text_style == text_style
                && !window.refreshing
                && !window.dirty_memos.contains(global_id)
                && !window.is_inspector_picking(cx)
                && !cx.has_active_drag()
                && state.hovers_unchanged(window)
            {
                window.keep_retained_layout(&state.layout_keys);
                let prepaint_start = window.prepaint_index();
                window.reuse_prepaint(state.prepaint_range.clone());
                cx.entities.extend_accessed(&state.accessed_entities);
                state.prepaint_range = prepaint_start..window.prepaint_index();
                return (None, state);
            }

            // Whatever is nested inside was drawn as part of this subtree, so
            // none of it can be reused once this subtree is built again.
            let refreshing = mem::replace(&mut window.refreshing, true);
            window.memo_stack.push(global_id.clone());
            let prepaint_start = window.prepaint_index();
            let recording = window.record_claimed_layout_keys();
            let build = self.build.take().expect("a memo is built once");
            let (element, accessed_entities) = cx.detect_accessed_entities(|cx| {
                let mut element = build(window, cx);
                element.layout_as_root(bounds.size.into(), window, cx);
                element.prepaint_at(bounds.origin, window, cx);
                element
            });
            let layout_keys = window.finish_recording_claimed_layout_keys(recording);
            let prepaint_end = window.prepaint_index();
            window.memo_stack.pop();
            window.refreshing = refreshing;

            (
                Some(element),
                MemoState {
                    key,
                    bounds,
                    content_mask,
                    text_style,
                    prepaint_range: prepaint_start..prepaint_end,
                    paint_range: PaintIndex::default()..PaintIndex::default(),
                    accessed_entities,
                    hover_dependencies: Vec::new(),
                    layout_keys,
                },
            )
        })
    }

    fn paint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        element: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let global_id = global_id.expect("a memo always has an id");
        window.with_element_state::<MemoState<K>, _>(global_id, |state, window| {
            let mut state = state.expect("a memo is prepainted before it is painted");
            let paint_start = window.paint_index();
            match element {
                Some(element) => {
                    let refreshing = mem::replace(&mut window.refreshing, true);
                    window.memo_stack.push(global_id.clone());
                    let dependencies_start = window.memo_hover_dependencies.len();
                    element.paint(window, cx);
                    state.hover_dependencies =
                        window.memo_hover_dependencies[dependencies_start..].to_vec();
                    window.memo_stack.pop();
                    window.refreshing = refreshing;
                }
                None => {
                    // The hovers were checked against last frame's hitboxes;
                    // this frame's could put something over the subtree. That
                    // is found out only now, too late to build it again, so it
                    // is built on the next frame, which is asked for.
                    if !state.hovers_unchanged(window) {
                        window
                            .memos_dirty_next_frame
                            .extend(window.memo_stack.iter().cloned());
                        window.memos_dirty_next_frame.insert(global_id.clone());
                        window.request_animation_frame();
                    }
                    window.reuse_paint(state.paint_range.clone());
                    // Memos around this one depend on these hovers too.
                    if !window.memo_stack.is_empty() {
                        window
                            .memo_hover_dependencies
                            .extend_from_slice(&state.hover_dependencies);
                    }
                }
            }
            state.paint_range = paint_start..window.paint_index();
            ((), state)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AppContext as _, Context, InteractiveElement as _, ParentElement as _, Render,
        TestAppContext, WindowHandle, div, prelude::FluentBuilder as _, px, size,
    };
    use std::{cell::Cell, rc::Rc};

    struct Memoized {
        key: u32,
        width: Pixels,
        builds: Rc<Cell<usize>>,
    }

    impl Render for Memoized {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let builds = self.builds.clone();
            let key = self.key;
            div().child(
                memo("memoized", key, move |_, _| {
                    builds.set(builds.get() + 1);
                    div().child(format!("key {key}"))
                })
                .w(self.width)
                .h(px(20.)),
            )
        }
    }

    fn window(cx: &mut TestAppContext) -> (WindowHandle<Memoized>, Rc<Cell<usize>>) {
        let builds = Rc::new(Cell::new(0));
        let window = cx.add_window({
            let builds = builds.clone();
            move |_, _| Memoized {
                key: 0,
                width: px(100.),
                builds,
            }
        });
        (window, builds)
    }

    fn draw(cx: &mut TestAppContext, window: WindowHandle<Memoized>) -> Vec<String> {
        cx.update_window(window.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
            window.describe_rendered_frame()
        })
        .unwrap()
    }

    fn change(
        cx: &mut TestAppContext,
        window: WindowHandle<Memoized>,
        f: impl FnOnce(&mut Memoized),
    ) {
        window
            .update(cx, |view, _, cx| {
                f(view);
                cx.notify();
            })
            .unwrap();
    }

    /// A memo is built the first time, and after that only when its key,
    /// its bounds or the window's refresh say it has to be; reused, it shows
    /// what it showed.
    #[test]
    fn a_memo_is_built_again_only_when_it_has_to_be() {
        let mut cx = TestAppContext::single();
        let (window, builds) = window(&mut cx);

        let first = draw(&mut cx, window);
        assert_eq!(builds.get(), 1);

        change(&mut cx, window, |_| {});
        let again = draw(&mut cx, window);
        assert_eq!(builds.get(), 1, "an unchanged key reuses the subtree");
        assert_eq!(first, again, "a reused subtree shows what it showed");

        change(&mut cx, window, |view| view.key = 1);
        draw(&mut cx, window);
        assert_eq!(builds.get(), 2, "a new key builds the subtree again");

        change(&mut cx, window, |view| view.width = px(150.));
        draw(&mut cx, window);
        assert_eq!(builds.get(), 3, "new bounds build the subtree again");

        cx.update_window(window.into(), |_, window, cx| {
            window.refresh();
            window.draw(cx).clear(cx);
        })
        .unwrap();
        assert_eq!(
            builds.get(),
            4,
            "a refreshed window builds every memo again"
        );

        cx.simulate_window_resize(window.into(), size(px(800.), px(600.)));
        draw(&mut cx, window);
        change(&mut cx, window, |_| {});
        draw(&mut cx, window);
        assert_eq!(
            builds.get(),
            5,
            "a resize refreshes once, then the memo is reused"
        );
    }

    struct Hoverable {
        builds: Rc<Cell<usize>>,
    }

    impl Render for Hoverable {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let builds = self.builds.clone();
            div().child(
                memo("hoverable", (), move |_, _| {
                    builds.set(builds.get() + 1);
                    div()
                        .size_full()
                        .bg(crate::black())
                        .hover(|style| style.bg(crate::white()))
                })
                .w(px(100.))
                .h(px(20.)),
            )
        }
    }

    /// A memo painted while the pointer was over something in it that has a
    /// hover style is built again once the pointer leaves, even though the
    /// element never saw the pointer arrive: the mouse started out over it.
    #[test]
    fn a_memo_is_built_again_when_a_hover_it_was_painted_by_changes() {
        let mut cx = TestAppContext::single();
        let builds = Rc::new(Cell::new(0));
        let window = cx.add_window({
            let builds = builds.clone();
            move |_, _| Hoverable { builds }
        });
        let draw = |cx: &mut TestAppContext| {
            cx.update_window(window.into(), |_, window, cx| {
                window.draw(cx).clear(cx);
                window.describe_rendered_frame()
            })
            .unwrap()
        };

        let hovered = draw(&mut cx);
        draw(&mut cx);
        assert_eq!(builds.get(), 1);

        cx.update_window(window.into(), |_, window, cx| {
            window.simulate_mouse_move(crate::point(px(500.), px(500.)), cx);
        })
        .unwrap();
        let left = draw(&mut cx);
        assert_eq!(builds.get(), 2, "the pointer leaving changes how it looks");
        assert_ne!(hovered, left);
    }

    /// A memo reused for a while keeps the layout nodes it was laid out with,
    /// so building it again, with a new key but the same shape, finds them
    /// all instead of allocating them anew.
    #[test]
    fn a_reused_memo_keeps_its_layout_nodes() {
        let mut cx = TestAppContext::single();
        let (window, builds) = window(&mut cx);
        draw(&mut cx, window);
        for _ in 0..3 {
            change(&mut cx, window, |_| {});
            draw(&mut cx, window);
        }
        assert_eq!(builds.get(), 1);

        cx.update_window(window.into(), |_, window, _| window.reset_layout_stats())
            .unwrap();
        change(&mut cx, window, |view| view.key = 7);
        draw(&mut cx, window);
        assert_eq!(builds.get(), 2);
        let stats = cx
            .update_window(window.into(), |_, window, _| window.layout_stats())
            .unwrap();
        assert_eq!(
            stats.nodes_created, 0,
            "the subtree's nodes should have been kept while it was reused"
        );
        assert!(stats.nodes_reused > 0);
    }

    struct Covered {
        builds: Rc<Cell<usize>>,
        covered: bool,
    }

    impl Render for Covered {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let builds = self.builds.clone();
            div()
                .relative()
                .size(px(300.))
                .child(
                    memo("covered", (), move |_, _| {
                        builds.set(builds.get() + 1);
                        div()
                            .size_full()
                            .bg(crate::black())
                            .hover(|style| style.bg(crate::white()))
                    })
                    .w(px(100.))
                    .h(px(20.)),
                )
                .when(self.covered, |this| {
                    this.child(div().absolute().top_0().left_0().size(px(200.)).occlude())
                })
        }
    }

    /// Whether a memo is still hovered is first checked against last frame's
    /// hitboxes, which cannot know of something drawn over it this frame. It
    /// is found out when the memo paints, and the memo is built on the next
    /// frame, which is asked for, without waiting for the pointer to move.
    #[test]
    fn a_memo_covered_while_hovered_is_built_on_the_next_frame() {
        let mut cx = TestAppContext::single();
        let builds = Rc::new(Cell::new(0));
        let window = cx.add_window({
            let builds = builds.clone();
            move |_, _| Covered {
                builds,
                covered: false,
            }
        });
        let draw = |cx: &mut TestAppContext| {
            cx.update_window(window.into(), |_, window, cx| {
                window.draw(cx).clear(cx);
                window.describe_rendered_frame()
            })
            .unwrap()
        };
        cx.update_window(window.into(), |_, window, cx| {
            window.simulate_mouse_move(crate::point(px(10.), px(10.)), cx);
        })
        .unwrap();
        let hovered = draw(&mut cx);
        draw(&mut cx);
        let builds_before = builds.get();

        // Covering it may draw a frame of its own; either way, without the
        // pointer moving, the memo is reused once, a frame is asked for, and
        // the next one builds it uncovered.
        window
            .update(&mut cx, |view, _, cx| {
                view.covered = true;
                cx.notify();
            })
            .unwrap();
        let frame_asked_for = |cx: &mut TestAppContext| {
            cx.update_window(window.into(), |_, window, _| {
                !window.next_frame_callbacks.borrow().is_empty()
            })
            .unwrap()
        };
        let mut asked_for_a_frame = frame_asked_for(&mut cx);
        let mut look = None;
        for _ in 0..3 {
            if builds.get() > builds_before {
                break;
            }
            asked_for_a_frame |= frame_asked_for(&mut cx);
            look = Some(draw(&mut cx));
        }
        assert_eq!(builds.get(), builds_before + 1);
        assert!(asked_for_a_frame, "a frame should be asked for to build it");
        assert_ne!(Some(hovered), look);
    }

    /// Keys of any type are equal only when they are of the same type and
    /// equal as that type.
    #[test]
    fn any_memo_keys_compare_by_type_and_value() {
        assert!(AnyMemoKey::new(Version(3)) == AnyMemoKey::new(Version(3)));
        assert!(AnyMemoKey::new(Version(3)) != AnyMemoKey::new(Version(4)));
        assert!(AnyMemoKey::new(3u64) != AnyMemoKey::new(Version(3)));
        assert!(
            AnyMemoKey::new(ContentHash::of(&("a", 1)))
                == AnyMemoKey::new(ContentHash::of(&("a", 1)))
        );
        assert!(ContentHash::of(&("a", 1)) != ContentHash::of(&("a", 2)));
    }
}
