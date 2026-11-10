//! Truncation against CoreText, whose shaping sets CJK punctuation half
//! width on its own and close to full width between ideographs: text cut by
//! the sum of its characters' widths still overflows its box. See gpui's
//! `fast::truncation`.

use std::{cell::RefCell, rc::Rc, sync::Arc};

use gpui::{
    AppContext as _, Context, IntoElement, ParentElement as _, Pixels, Render, Styled as _,
    StyledText, TestAppContext, TestDispatcher, TextLayout, Window, div, px,
};

use crate::AppleTextSystem;

const TEXT: &str = "订单：编号（甲）已付款，金额：一千二百元，收货地址：城东区（近公园）三号楼";

struct Truncated {
    width: Pixels,
    layout: Rc<RefCell<Option<TextLayout>>>,
}

impl Render for Truncated {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let text = StyledText::new(TEXT);
        *self.layout.borrow_mut() = Some(text.layout().clone());
        div()
            .w(self.width)
            .font_family(".SystemUIFont")
            .text_size(px(12.))
            .truncate()
            .child(text)
    }
}

/// A Chinese sentence with full-width punctuation, truncated by CoreText's
/// shaping, ends in an ellipsis that fits in its box.
#[test]
fn chinese_with_full_width_punctuation_truncates_within_its_box() {
    for width in [120., 160., 200., 240., 280.] {
        let mut cx = TestAppContext::build_with_text_system(
            TestDispatcher::new(0),
            None,
            Arc::new(AppleTextSystem::new()),
        );
        let window = cx.add_window(move |_, _| Truncated {
            width: px(width),
            layout: Default::default(),
        });
        cx.update_window(window.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
        })
        .unwrap();
        let (text, line_widths) = window
            .update(&mut cx, |view, _, _| {
                let layout = view.layout.borrow().clone().unwrap();
                let widths = layout
                    .line_layouts()
                    .iter()
                    .map(|line| line.unwrapped_layout.width)
                    .collect::<Vec<_>>();
                (layout.text(), widths)
            })
            .unwrap();
        assert!(
            text.ends_with('…'),
            "{width}px: the text should be cut short: {text:?}"
        );
        assert!(
            line_widths.iter().all(|line| *line <= px(width)),
            "{width}px: the shaped line overflows its box, clipping the ellipsis: {text:?} {line_widths:?}"
        );
    }
}
