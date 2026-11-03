//! Late layout can put a rebuilt child's accesses before a copied sibling.

use crate::{
    AnyElement, App, AppContext as _, Bounds, Context, Element, ElementId, Entity, GlobalElementId,
    InspectorElementId, InteractiveElement as _, IntoElement, LayoutId, Pixels, Render,
    Styled as _, TestAppContext, Window, div, px,
};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

type Slot = Rc<Cell<Option<LayoutId>>>;
type StateId = Rc<RefCell<Option<GlobalElementId>>>;
struct Marker(Rc<Cell<usize>>);
impl Drop for Marker {
    fn drop(&mut self) {
        self.0.set(self.0.get() - 1);
    }
}
fn touch(window: &mut Window, id: &GlobalElementId, live: &Rc<Cell<usize>>) {
    window.with_element_state(id, |old: Option<Marker>, _| {
        let marker = old.unwrap_or_else(|| {
            live.set(live.get() + 1);
            Marker(live.clone())
        });
        ((), marker)
    });
}

enum Body {
    Row([Slot; 2], [AnyElement; 2], bool),
    Forward(AnyElement),
    State(Leaf),
}
struct TestElement(Option<ElementId>, Body);
impl IntoElement for TestElement {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}
impl Element for TestElement {
    type RequestLayoutState = ();
    type PrepaintState = ();
    fn id(&self) -> Option<ElementId> {
        self.0.clone()
    }
    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }
    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let node = match &mut self.1 {
            Body::Row(slots, children, late_layout) => {
                // Connect slots to the parent so the independent-root guard
                // does not reject the splice before state copying is reached.
                let nodes = slots
                    .iter()
                    .enumerate()
                    .map(|(index, slot)| {
                        let node = div()
                            .id(ElementId::NamedInteger("slot".into(), index as u64))
                            .size(px(20.))
                            .into_any_element()
                            .request_layout(window, cx);
                        slot.set(Some(node));
                        node
                    })
                    .collect::<Vec<_>>();
                if !*late_layout {
                    for child in children {
                        child.request_layout(window, cx);
                    }
                }
                window.request_layout(Default::default(), nodes, cx)
            }
            Body::Forward(child) => child.request_layout(window, cx),
            Body::State(leaf) => {
                *leaf.state_id.borrow_mut() = id.filter(|_| leaf.keep).cloned();
                if leaf.keep {
                    touch(window, id.unwrap(), &leaf.live);
                }
                leaf.slot.get().unwrap()
            }
        };
        (node, ())
    }
    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        match &mut self.1 {
            Body::Row(_, children, late_layout) => {
                // Batch late layout before prepaint so a sibling's accesses
                // can precede the other's recorded prepaint boundaries.
                if *late_layout {
                    for child in children.iter_mut() {
                        child.request_layout(window, cx);
                    }
                }
                for child in children {
                    child.prepaint(window, cx);
                }
            }
            Body::Forward(child) => {
                child.prepaint(window, cx);
            }
            Body::State(leaf) if leaf.keep && leaf.prepaint_touch => {
                touch(window, id.unwrap(), &leaf.live);
                if let Some((sibling_id, sibling_live)) = &leaf.prepaint_sibling
                    && let Some(sibling_id) = sibling_id.borrow().as_ref()
                {
                    // An access owned by B also falls inside A's recorded range.
                    touch(window, sibling_id, sibling_live);
                }
            }
            _ => {}
        }
    }
    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        match &mut self.1 {
            Body::Row(_, children, _) => children
                .iter_mut()
                .for_each(|child| child.paint(window, cx)),
            Body::Forward(child) => child.paint(window, cx),
            _ => {}
        }
    }
}

#[derive(Clone)]
struct Leaf {
    slot: Slot,
    live: Rc<Cell<usize>>,
    keep: bool,
    prepaint_touch: bool,
    state_id: StateId,
    prepaint_sibling: Option<(StateId, Rc<Cell<usize>>)>,
}
impl Render for Leaf {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        TestElement(Some("state".into()), Body::State(self.clone()))
    }
}
struct Root {
    slots: [Slot; 2],
    views: [Entity<Leaf>; 2],
    renders: Rc<Cell<usize>>,
    late_layout: bool,
}
impl Render for Root {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        self.renders.set(self.renders.get() + 1);
        TestElement(
            None,
            Body::Row(
                self.slots.clone(),
                std::array::from_fn(|index| {
                    TestElement(
                        Some(ElementId::NamedInteger("forward".into(), index as u64)),
                        Body::Forward(self.views[index].clone().into_any_element()),
                    )
                    .into_any_element()
                }),
                self.late_layout,
            ),
        )
    }
}

#[test]
fn late_layout_keeps_state_ranges_valid_without_rendering_the_parent() {
    for (retention, late_layout, foreign) in [
        (false, true, false),
        (true, true, false),
        (false, false, true),
        (true, false, true),
        (false, false, false),
        (true, false, false),
    ] {
        let mut cx = TestAppContext::single();
        let slots: [Slot; 2] = std::array::from_fn(|_| Rc::new(Cell::new(None)));
        let live: [Rc<Cell<usize>>; 2] = std::array::from_fn(|_| Rc::new(Cell::new(0)));
        let state_ids: [StateId; 2] = std::array::from_fn(|_| Rc::new(RefCell::new(None)));
        let renders = Rc::new(Cell::new(0));
        let views = std::array::from_fn(|index| {
            cx.new(|_| Leaf {
                slot: slots[index].clone(),
                live: live[index].clone(),
                keep: true,
                prepaint_touch: index == 0,
                state_id: state_ids[index].clone(),
                prepaint_sibling: (foreign && index == 0)
                    .then(|| (state_ids[1].clone(), live[1].clone())),
            })
        });
        let window = cx.add_window(|window, _| {
            window.set_view_retention(retention);
            Root {
                slots,
                views: views.clone(),
                renders: renders.clone(),
                late_layout,
            }
        });
        let update = |index: usize, keep, cx: &mut TestAppContext| {
            views[index].update(cx, |leaf, cx| {
                leaf.keep = keep;
                cx.notify();
            });
        };
        let draw = |cx: &mut TestAppContext| {
            cx.update_window(window.into(), |_, window, cx| window.draw(cx).clear(cx))
                .unwrap();
        };
        let before = renders.get();
        for _ in 0..3 {
            draw(&mut cx);
        }
        assert_eq!(live[1].get(), 1);
        update(1, false, &mut cx);
        assert_eq!(live[1].get(), 0, "removed state must be dropped");
        assert_eq!(live[0].get(), 1, "the sibling state must survive");
        draw(&mut cx);
        update(0, true, &mut cx); // Reuse the copied sibling's remapped range.
        assert_eq!(live[1].get(), 0);
        update(1, true, &mut cx);
        assert_eq!(live[1].get(), 1);
        draw(&mut cx);
        update(1, false, &mut cx);
        assert_eq!(live[1].get(), 0, "idle reuse must preserve filtering");
        if retention {
            assert_eq!(renders.get(), before, "the parent must not rerender");
        }
    }
}
