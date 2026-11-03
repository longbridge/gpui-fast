//! Tests of drawing a view from last frame around the nested views built
//! again in it.

use crate::fast::splice::kept_keys;
use crate::prelude::FluentBuilder as _;
use crate::{
    AppContext as _, Context, ElementId, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, Styled as _, TestAppContext, Window, div, px,
};
use collections::FxHashSet;
use rand::{Rng as _, SeedableRng as _, rngs::StdRng};

/// What `kept_keys` has to give: every key that is no gap's, in order.
fn kept_keys_naively(keys: &[u64], gaps: &[Vec<u64>]) -> Vec<u64> {
    let gap_keys: FxHashSet<u64> = gaps.iter().flatten().copied().collect();
    keys.iter()
        .copied()
        .filter(|key| !gap_keys.contains(key))
        .collect()
}

#[test]
fn kept_keys_are_the_keys_of_no_gap() {
    let mut scratch = FxHashSet::default();
    for seed in 0..2000 {
        let mut rng = StdRng::seed_from_u64(seed);
        // A small key space, so that keys recorded twice come up.
        let space = rng.random_range(4..200u64);
        let len = rng.random_range(0..60);
        let keys: Vec<u64> = (0..len).map(|_| rng.random_range(0..space)).collect();
        let mut gaps = Vec::new();
        let mut cursor = 0;
        for _ in 0..rng.random_range(0..4) {
            let gap = match rng.random_range(0..4) {
                // A stretch of the view's keys, as a nested view's usually is.
                0 | 1 if cursor < keys.len() => {
                    let start = rng.random_range(cursor..keys.len());
                    let end = rng.random_range(start..=keys.len());
                    cursor = end;
                    keys[start..end].to_vec()
                }
                // Keys the view never had, or had elsewhere.
                2 => (0..rng.random_range(0..8))
                    .map(|_| rng.random_range(0..space * 2))
                    .collect(),
                _ => Vec::new(),
            };
            gaps.push(gap);
        }
        let kept = kept_keys(&keys, gaps.iter().map(Vec::as_slice), &mut scratch);
        assert_eq!(
            kept,
            kept_keys_naively(&keys, &gaps),
            "seed {seed}: {keys:?} without {gaps:?}"
        );
        assert!(scratch.is_empty());
    }
}

struct Outer {
    middle: Entity<Middle>,
    opacity: f32,
}

impl Render for Outer {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .opacity(self.opacity)
            .child(self.middle.clone())
    }
}

struct Middle {
    inner: Entity<Nest>,
}

impl Render for Middle {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.inner.clone())
    }
}

struct Nest {
    inner: Entity<Inner>,
}

impl Render for Nest {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().child(self.inner.clone())
    }
}

struct Inner {
    leaf: bool,
}

impl Render for Inner {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        // Sized by the leaf, so that removing it lays the view around out
        // differently, and that view is built after all.
        div().when(self.leaf, |this| this.child(div().id("leaf").size(px(10.))))
    }
}

/// A nested view built again in its gap keeps no state of an element it no
/// longer has, even where the views around it, laid out differently by it,
/// are built after all: the state is dropped at the end of the frame, as
/// upstream drops the state of every element a frame does not draw, and an
/// element with that id drawn later starts afresh.
#[test]
fn a_view_drawn_around_a_rebuilt_view_keeps_no_state_of_its_removed_elements() {
    let mut cx = TestAppContext::single();
    let window = cx.add_window(|_, cx| {
        let inner = cx.new(|_| Inner { leaf: true });
        let inner = cx.new(|_| Nest { inner });
        Outer {
            middle: cx.new(|_| Middle { inner }),
            opacity: 1.0,
        }
    });
    let inner = window
        .update(&mut cx, |outer, _, cx| {
            let nest = outer.middle.read(cx).inner.clone();
            nest.read(cx).inner.clone()
        })
        .unwrap();
    let leaf_states = |cx: &mut TestAppContext| {
        cx.update_window(window.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
            window
                .rendered_frame
                .element_states
                .keys()
                .filter(|(id, _)| id.last() == Some(&ElementId::from("leaf")))
                .count()
        })
        .unwrap()
    };
    assert_ne!(leaf_states(&mut cx), 0, "the leaf keeps an element state");

    // Rebuild clean descendants during prepaint because their inherited
    // opacity changed, then splice around them on the following frame.
    window
        .update(&mut cx, |outer, _, cx| {
            outer.opacity = 0.5;
            cx.notify();
        })
        .unwrap();
    assert_ne!(leaf_states(&mut cx), 0);
    let live_nodes = |cx: &mut TestAppContext| {
        cx.update_window(window.into(), |_, window, _| {
            let stats = window.layout_stats();
            stats.nodes_created - stats.nodes_freed
        })
        .unwrap()
    };
    let before_removal = live_nodes(&mut cx);

    inner.update(&mut cx, |inner, cx| {
        inner.leaf = false;
        cx.notify();
    });
    assert_eq!(leaf_states(&mut cx), 0);
    assert_eq!(leaf_states(&mut cx), 0);
    assert_eq!(live_nodes(&mut cx), before_removal - 1);
}

mod ancestor_reuse {
    use crate::{
        AppContext as _, Context, Entity, Hsla, IntoElement, ParentElement as _, Render,
        Styled as _, TestAppContext, VisualTestContext, Window, div, px, rgb,
    };
    use std::{cell::Cell, rc::Rc};

    struct Child {
        value: usize,
        model: Entity<usize>,
    }
    impl Render for Child {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let value = self.value + *self.model.read(cx);
            div().size(px(24.)).bg(rgb(value as u32))
        }
    }
    struct Parent {
        child: Entity<Child>,
        renders: Rc<Cell<usize>>,
    }
    impl Render for Parent {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            self.renders.set(self.renders.get() + 1);
            div().size_full().child(self.child.clone())
        }
    }

    fn draw_child(cx: &mut VisualTestContext, value: usize) {
        cx.update(|window, cx| {
            window.draw(cx).clear(cx);
            let quads = window.painted_quads();
            assert_eq!(quads.len(), 1);
            assert_eq!(
                quads[0].background.as_solid(),
                Some(Hsla::from(rgb(value as u32)))
            );
        });
    }

    #[crate::test]
    fn spliced_children_keep_ancestors_reusable(cx: &mut TestAppContext) {
        let renders = Rc::new(Cell::new(0));
        let model = cx.new(|_| 0usize);
        let (parent, cx) = cx.add_window_view(|_, cx| Parent {
            child: cx.new(|_| Child {
                value: 0,
                model: model.clone(),
            }),
            renders: renders.clone(),
        });
        let child = parent.read_with(cx, |parent, _| parent.child.clone());
        draw_child(cx, 0);
        let initial = renders.get();
        for value in 1..=4 {
            child.update(cx, |child, cx| {
                child.value = value;
                cx.notify();
            });
            // Include an idle frame: stale aggregate checkpoints used to rebuild
            // the ancestor after the successful partial update.
            for _ in 0..2 {
                draw_child(cx, value);
            }
            assert_eq!(renders.get(), initial);
        }

        // Advancing the aggregate must not hide a model written without notify.
        // The child's own checkpoint still makes it rebuild under a notified parent.
        model.update(cx, |model, _| *model = 10);
        parent.update(cx, |_, cx| cx.notify());
        draw_child(cx, 14);
        let after_parent = renders.get();
        model.update(cx, |model, cx| {
            *model = 20;
            cx.notify();
        });
        for _ in 0..2 {
            draw_child(cx, 24);
        }
        assert_eq!(renders.get(), after_parent);
    }
}
