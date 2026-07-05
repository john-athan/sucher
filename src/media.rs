// Shared pixel-display pane built on ratatui-image. Queries the terminal once
// for its graphics protocol (kitty / iterm2 / sixel, with unicode-halfblock
// fallback) and renders DynamicImages into a ratatui area. Used by the image,
// PDF, and video viewers.

use image::DynamicImage;
use ratatui::layout::Rect;
use ratatui::Frame;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::StatefulProtocol;
use ratatui_image::{Resize, StatefulImage};
use std::io;

pub struct ImagePane {
    picker: Picker,
    proto: Option<StatefulProtocol>,
}

impl ImagePane {
    /// Must be called before entering the alternate screen — it queries the
    /// terminal over stdio.
    pub fn new() -> io::Result<Self> {
        let picker = Picker::from_query_stdio()
            .map_err(|e| io::Error::other(format!("graphics probe failed: {e}")))?;
        Ok(Self {
            picker,
            proto: None,
        })
    }

    pub fn set(&mut self, img: DynamicImage) {
        self.proto = Some(self.picker.new_resize_protocol(img));
    }

    pub fn render(&mut self, f: &mut Frame, area: Rect) {
        if let Some(p) = self.proto.as_mut() {
            let widget = StatefulImage::<StatefulProtocol>::default().resize(Resize::Fit(None));
            f.render_stateful_widget(widget, area, p);
        }
    }
}
