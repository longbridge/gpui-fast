//! The accessibility tree a window builds while drawing views and scroll
//! layers again from the last frame must be the tree it would build drawing
//! every frame from scratch. See `crate::fast::a11y`.
//!
//! The oracles (`oracle.rs`, `layers_oracle.rs`) compare whole trees over
//! random histories with [`a11y_snapshot`] and [`a11y_difference`]; the tests
//! here pin down single cases.

use std::fmt::Write as _;

use accesskit::{NodeId, Role};

use crate::{
    Context, Entity, FocusHandle, IntoElement, ParentElement as _, Render, Styled as _,
    TestAppContext, Window, div, prelude::*, px,
};

/// One node of a window's accessibility tree, named by where it is rather
/// than by its id: two windows drawing the same views have different view
/// ids, so different node ids.
#[derive(Debug)]
pub(super) struct A11yEntry {
    /// The child indices from the root down to the node.
    path: String,
    /// The node, its children and bounds left out.
    node: String,
    rect: Option<accesskit::Rect>,
    /// What a click on it would hit, in logical pixels.
    click_bounds: Option<crate::Bounds<crate::Pixels>>,
    focusable: bool,
    actions: Vec<String>,
}

/// A window's last accessibility tree, as [`A11yEntry`]s in depth-first
/// order, after the path of the focused node.
pub(super) fn a11y_snapshot(window: &Window) -> (String, Vec<A11yEntry>) {
    let tree = crate::fast::a11y::last_tree(window).expect("accessibility is active");
    let nodes: collections::FxHashMap<NodeId, &accesskit::Node> =
        tree.nodes.iter().map(|(id, node)| (*id, node)).collect();
    assert_eq!(nodes.len(), tree.nodes.len(), "a node id twice in the tree");
    let mut entries = Vec::new();
    let mut focus = String::from("unreachable");
    let mut stack = vec![(crate::window::a11y::ROOT_NODE_ID, String::from("r"))];
    while let Some((id, path)) = stack.pop() {
        let node = nodes[&id];
        if id == tree.focus {
            focus = path.clone();
        }
        for (ix, child) in node.children().iter().enumerate().rev() {
            stack.push((*child, format!("{path}.{ix}")));
        }
        let mut bare = node.clone();
        bare.clear_children();
        bare.clear_bounds();
        let mut actions: Vec<String> = window
            .a11y
            .action_listeners
            .get(&id)
            .map(|listeners| {
                listeners
                    .iter()
                    .map(|(action, _)| format!("{action:?}"))
                    .collect()
            })
            .unwrap_or_default();
        actions.sort();
        entries.push(A11yEntry {
            path,
            node: format!("{bare:?}"),
            rect: node.bounds(),
            click_bounds: window.a11y.node_bounds.get(&id).copied(),
            focusable: window.a11y.focus_ids.contains_key(&id),
            actions,
        });
    }
    assert_eq!(
        entries.len(),
        tree.nodes.len(),
        "a node in the tree that is not reachable from its root"
    );
    (focus, entries)
}

/// How `actual` differs from `expected`, bounds compared to within
/// `epsilon` device pixels, if it does.
pub(super) fn a11y_difference(
    actual: &(String, Vec<A11yEntry>),
    expected: &(String, Vec<A11yEntry>),
    epsilon: f64,
) -> Option<String> {
    let mut out = String::new();
    if actual.0 != expected.0 {
        writeln!(out, "focus at {} instead of {}", actual.0, expected.0).unwrap();
    }
    let close = |a: f64, b: f64| (a - b).abs() <= epsilon;
    let rects_match = |a: &Option<accesskit::Rect>, b: &Option<accesskit::Rect>| match (a, b) {
        (Some(a), Some(b)) => {
            close(a.x0, b.x0) && close(a.y0, b.y0) && close(a.x1, b.x1) && close(a.y1, b.y1)
        }
        (None, None) => true,
        _ => false,
    };
    let bounds_match = |a: &Option<crate::Bounds<crate::Pixels>>,
                        b: &Option<crate::Bounds<crate::Pixels>>| match (
        a, b,
    ) {
        (Some(a), Some(b)) => {
            close(a.origin.x.0 as f64, b.origin.x.0 as f64)
                && close(a.origin.y.0 as f64, b.origin.y.0 as f64)
                && close(a.size.width.0 as f64, b.size.width.0 as f64)
                && close(a.size.height.0 as f64, b.size.height.0 as f64)
        }
        (None, None) => true,
        _ => false,
    };
    for (a, e) in actual.1.iter().zip(&expected.1) {
        if a.path != e.path
            || a.node != e.node
            || !rects_match(&a.rect, &e.rect)
            || !bounds_match(&a.click_bounds, &e.click_bounds)
            || a.focusable != e.focusable
            || a.actions != e.actions
        {
            writeln!(out, "  actual:   {a:?}\n  expected: {e:?}").unwrap();
            break;
        }
    }
    if actual.1.len() != expected.1.len() {
        writeln!(
            out,
            "{} nodes instead of {}",
            actual.1.len(),
            expected.1.len()
        )
        .unwrap();
    }
    (!out.is_empty()).then_some(out)
}

struct Item {
    label: usize,
    /// Whether its synthetic node has bounds of its own.
    placed: bool,
}

impl Render for Item {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("item")
            .role(Role::ListItem)
            .aria_label(format!("item {}", self.label))
            .when(self.label % 4 == 1, |this| this.aria_active_descendant())
            .h(px(20.))
            .child(
                div()
                    .id("inner")
                    .role(Role::Button)
                    .on_a11y_action(accesskit::Action::Click, |_, _, _| {})
                    .size(px(10.))
                    .a11y_synthetic_children({
                        let placed = self.placed;
                        move |builder| {
                            let id = builder.synthetic_node_id(0);
                            let mut node = accesskit::Node::new(Role::Image);
                            if placed {
                                node.set_bounds(accesskit::Rect {
                                    x0: 1.,
                                    y0: 2.,
                                    x1: 3.,
                                    y1: 4.,
                                });
                            }
                            builder.push_child(id, node);
                        }
                    }),
            )
    }
}

struct ListView {
    items: Vec<Entity<Item>>,
    focus: FocusHandle,
    title: usize,
}

impl Render for ListView {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("list")
            .role(Role::List)
            .track_focus(&self.focus)
            .aria_label(format!("list {}", self.title))
            .flex()
            .flex_col()
            .children(self.items.iter().cloned())
    }
}

fn snapshot(cx: &mut TestAppContext, window: crate::WindowHandle<ListView>) -> String {
    cx.update_window(window.into(), |_, window, cx| {
        window.draw(cx).clear(cx);
        let (focus, entries) = a11y_snapshot(window);
        let mut out = format!("focus {focus}\n");
        for entry in entries {
            writeln!(out, "{entry:?}").unwrap();
        }
        out
    })
    .unwrap()
}

/// Draws the same history in a window retaining views and one refreshed
/// every frame, comparing their trees.
#[crate::test]
fn reused_and_spliced_views_keep_their_accessibility_nodes(cx: &mut TestAppContext) {
    let open = |cx: &mut TestAppContext| {
        cx.add_window(|_, cx| ListView {
            items: (0..4)
                .map(|label| {
                    cx.new(|_| Item {
                        label,
                        placed: true,
                    })
                })
                .collect(),
            focus: cx.focus_handle(),
            title: 0,
        })
    };
    let retained = open(cx);
    let scratch = open(cx);
    for window in [retained, scratch] {
        cx.update_window(window.into(), |_, window, _| {
            window.set_a11y_active_for_tests(true)
        })
        .unwrap();
    }
    let step = |cx: &mut TestAppContext, f: &dyn Fn(&mut ListView, &mut Context<ListView>)| {
        for window in [retained, scratch] {
            window.update(cx, |view, _, cx| f(view, cx)).unwrap();
        }
        cx.update_window(scratch.into(), |_, window, _| window.refresh())
            .unwrap();
        let expected = snapshot(cx, scratch);
        let actual = snapshot(cx, retained);
        assert_eq!(actual, expected);
        cx.update_window(retained.into(), |_, window, _| {
            window.rendered_frame.retained.reused_any()
        })
        .unwrap()
    };
    step(cx, &|_, _| {});
    // Nothing changed: every item is drawn from the last frame.
    assert!(step(cx, &|_, cx| cx.notify()));
    // One item changed: the list is drawn around it.
    assert!(step(cx, &|view, cx| {
        view.items[2].update(cx, |item, cx| {
            item.label = 6;
            cx.notify();
        })
    }));
    // The list focused: its items claim to be its active descendant.
    for window in [retained, scratch] {
        cx.update_window(window.into(), |view, window, cx| {
            let focus = view.downcast::<ListView>().unwrap().read(cx).focus.clone();
            window.focus(&focus, cx);
        })
        .unwrap();
    }
    step(cx, &|_, _| {});
    assert!(step(cx, &|view, cx| {
        view.items[1].update(cx, |item, cx| {
            item.label = 7;
            cx.notify();
        })
    }));
    assert!(step(cx, &|view, cx| {
        view.title += 1;
        cx.notify()
    }));
}

/// A frame drawn after accessibility was turned on draws nothing from a
/// frame drawn without it, which logged no accessibility nodes.
#[crate::test]
fn turning_accessibility_on_builds_every_view(cx: &mut TestAppContext) {
    let window = cx.add_window(|_, cx| ListView {
        items: (0..3)
            .map(|label| {
                cx.new(|_| Item {
                    label,
                    placed: true,
                })
            })
            .collect(),
        focus: cx.focus_handle(),
        title: 0,
    });
    cx.update_window(window.into(), |_, window, cx| {
        window.draw(cx).clear(cx);
        window.draw(cx).clear(cx);
        crate::fast::a11y::set_active_flag(&window.a11y, true);
        window.draw(cx).clear(cx);
        assert!(!window.rendered_frame.retained.reused_any());
        let (_, entries) = a11y_snapshot(window);
        // The window, the list, and an item, its button and its image each.
        assert_eq!(entries.len(), 2 + 3 * 3);
        window.draw(cx).clear(cx);
        assert!(window.rendered_frame.retained.reused_any());
        let (_, again) = a11y_snapshot(window);
        assert_eq!(format!("{again:?}"), format!("{entries:?}"));
    })
    .unwrap();
}

/// A frame drawn with accessibility active keeps the tree it sent in
/// `A11yDebug::last_tree_update`, which views drawn again read their
/// finished nodes from.
#[crate::test]
fn a11y_frame_keeps_the_tree_it_sent(cx: &mut TestAppContext) {
    let window = cx.add_window(|_, cx| ListView {
        items: Vec::new(),
        focus: cx.focus_handle(),
        title: 0,
    });
    cx.update_window(window.into(), |_, window, cx| {
        crate::fast::a11y::set_active_flag(&window.a11y, true);
        window.draw(cx).clear(cx);
        assert!(crate::fast::a11y::last_tree(window).is_some());
    })
    .unwrap();
}

/// A page scrolling rows, some of them holding views, with accessibility
/// nodes.
struct ScrollPage {
    handle: crate::ScrollHandle,
    items: Vec<Entity<Item>>,
    placed: bool,
}

impl Render for ScrollPage {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().bg(crate::white()).child(
            div()
                .id("scroller")
                .role(Role::ScrollView)
                .overflow_y_scroll()
                .track_scroll(&self.handle)
                .w(px(200.))
                .h(px(100.))
                .bg(crate::white())
                .children((0..40u32).map(|row| {
                    div()
                        .id(("row", row as usize))
                        .role(Role::Row)
                        .aria_label(format!("row {row}"))
                        .h(px(20.))
                        .bg(crate::rgb(0x100000 + row * 0x10))
                        .a11y_synthetic_children({
                            let placed = self.placed;
                            move |builder| {
                                let id = builder.synthetic_node_id(0);
                                let mut node = accesskit::Node::new(Role::Image);
                                if placed && let Some(bounds) = builder.parent_node().bounds() {
                                    node.set_bounds(bounds);
                                }
                                builder.push_child(id, node);
                            }
                        })
                        .when(row % 8 == 3, |this| {
                            this.child(self.items[row as usize / 8].clone())
                        })
                })),
        )
    }
}

/// A composited scroll layer adds its content's accessibility nodes moved
/// by the scroll since it was painted, as drawing the content would.
#[crate::test]
fn a_composited_layer_keeps_its_content_accessibility_nodes(cx: &mut TestAppContext) {
    assert_eq!(
        scroll_page_frames(cx, false) > 0,
        crate::fast::layers::COMPILED,
        "no frame composited the layer"
    );
}

/// Content holding synthetic nodes its code placed is painted again on
/// every scrolled frame rather than moved: nothing says the code places them
/// where its element is.
#[crate::test]
fn a_layer_holding_placed_synthetic_nodes_is_not_composited(cx: &mut TestAppContext) {
    assert_eq!(scroll_page_frames(cx, true), 0);
}

/// Scrolls a [`ScrollPage`] with layers and one without, comparing their
/// trees every frame, and returns how many frames composited the layer.
fn scroll_page_frames(cx: &mut TestAppContext, placed: bool) -> u64 {
    if !crate::fast::layers::COMPILED {
        return 0;
    }
    let open = |cx: &mut TestAppContext, layers: bool| {
        let window = cx.add_window(|_, cx| ScrollPage {
            handle: crate::ScrollHandle::new(),
            items: (0..5)
                .map(|label| cx.new(|_| Item { label, placed }))
                .collect(),
            placed,
        });
        cx.update_window(window.into(), |_, window, cx| {
            window.set_scroll_layers(layers);
            window.set_a11y_active_for_tests(true);
            window.draw(cx).clear(cx);
        })
        .unwrap();
        window
    };
    let layered = open(cx, true);
    let plain = open(cx, false);
    let mut composited = 0;
    for step in 0..30 {
        let dy = if step < 20 { -7.5 } else { 11. };
        let mut snapshots = Vec::new();
        for window in [layered, plain] {
            cx.update_window(window.into(), |_, window, cx| {
                let before = window.layout_stats();
                window.dispatch_event(
                    crate::PlatformInput::ScrollWheel(crate::ScrollWheelEvent {
                        position: crate::point(px(20.), px(20.)),
                        delta: crate::ScrollDelta::Pixels(crate::point(px(0.), px(dy))),
                        modifiers: Default::default(),
                        touch_phase: crate::TouchPhase::Moved,
                    }),
                    cx,
                );
                window.draw(cx).clear(cx);
                let after = window.layout_stats();
                if window.fast_layers.enabled {
                    composited += (after.layer_frames_composited - before.layer_frames_composited)
                        - (after.layer_frames_repainted - before.layer_frames_repainted);
                }
                snapshots.push(a11y_snapshot(window));
            })
            .unwrap();
        }
        if let Some(difference) = a11y_difference(&snapshots[0], &snapshots[1], 1e-3) {
            panic!("step {step}: the trees differ:\n{difference}");
        }
    }
    composited
}
