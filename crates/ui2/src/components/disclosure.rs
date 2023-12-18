use gpui::ClickEvent;

use crate::{prelude::*, Color, Icon, IconButton, IconSize};

#[derive(IntoElement)]
pub struct Disclosure {
    id: ElementId,
    is_open: bool,
    on_toggle: Option<Box<dyn Fn(&ClickEvent, &mut WindowContext) + 'static>>,
}

impl Disclosure {
    pub fn new(id: impl Into<ElementId>, is_open: bool) -> Self {
        Self {
            id: id.into(),
            is_open,
            on_toggle: None,
        }
    }

    pub fn on_toggle(
        mut self,
        handler: impl Into<Option<Box<dyn Fn(&ClickEvent, &mut WindowContext) + 'static>>>,
    ) -> Self {
        self.on_toggle = handler.into();
        self
    }
}

impl RenderOnce for Disclosure {
    type Rendered = IconButton;

    fn render(self, _cx: &mut WindowContext) -> Self::Rendered {
        IconButton::new(
            self.id,
            match self.is_open {
                true => Icon::ChevronDown,
                false => Icon::ChevronRight,
            },
        )
        .icon_color(Color::Muted)
        .icon_size(IconSize::Small)
        .when_some(self.on_toggle, move |this, on_toggle| {
            this.on_click(move |event, cx| on_toggle(event, cx))
        })
    }
}
