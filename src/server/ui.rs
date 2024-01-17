use slint::{Model, ModelNotify, ModelTracker};
use std::cell::RefCell;
use std::sync::Arc;
use std::thread;
use tower_lsp::lsp_types::Range;
use tracing::debug;
use typst::model::Document;

// TODO: why do we panic when closing the window??
//       -> If I comment out the tracing_subscriber::registery().init() thing the crash goes away
//       (in src/logging.rs)

// Model that lazily converts pages of a typst `Document` to a `slint::image` when they are scrolled into view.
// The usefulness of this comes from slint's `ListView` only instantiating elements that are visible.
pub struct LazyImagesModel {
    images: RefCell<Vec<Option<slint::Image>>>,
    document: RefCell<Option<Arc<Document>>>,
    frame_hashes: RefCell<Vec<u128>>,
    source: RefCell<Option<typst::syntax::Source>>,
    notify: ModelNotify,
    zoom: RefCell<f32>,
    main_window_weak: slint::Weak<MainWindow>,
}

impl LazyImagesModel {
    pub fn new(main_window_weak: slint::Weak<MainWindow>) -> Self {
        LazyImagesModel {
            images: RefCell::new(Vec::new()),
            document: RefCell::new(None),
            frame_hashes: RefCell::new(Vec::new()),
            source: RefCell::new(None),
            notify: Default::default(),
            zoom: RefCell::new(1.0),
            main_window_weak,
        }
    }

    fn slint_workaround_redraw(&self) {
        // TODO: slint bug workaround
        // https://github.com/slint-ui/slint/issues/3125
        // not sure. the bug fix mentioned there doesn't seem to fix it?
        // only the workaround mentioned there:
        self.main_window_weak
            .upgrade_in_event_loop(move |main_window| {
                main_window.window().request_redraw();
            })
            .unwrap();
    }

    pub fn set_doc(
        &self,
        new_doc: Arc<Document>,
        new_source: typst::syntax::Source,
        first_change_range: Option<Range>,
    ) {
        let new_len = new_doc.pages.len();

        let old_hashes = self.frame_hashes.replace(
            new_doc
                .pages
                .iter()
                .map(|frame| typst::util::hash128(frame))
                .collect(),
        );
        let old_document = self.document.replace(Some(new_doc));
        let old_source = self.source.replace(Some(new_source));
        *self.images.borrow_mut() = std::iter::repeat_with(|| None).take(new_len).collect();
        self.notify.reset();

        self.slint_workaround_redraw();

        // Find first change and scroll to it
        if let Some(range) = first_change_range {
            let document = self.document.borrow();
            let document = document.as_ref().unwrap();

            let source = self.source.borrow();
            let source = source.as_ref().unwrap();

            // Convert lsp range (line + character) to byte range
            // TODO: Does that work with non-ascii?
            let range = std::ops::Range {
                start: source
                    .line_column_to_byte(range.start.line as usize, range.start.character as usize)
                    .unwrap_or_else(|| source.len_bytes() - 1),
                end: source
                    .line_column_to_byte(range.end.line as usize, range.end.character as usize)
                    .unwrap_or_else(|| source.len_bytes() - 1),
            };
            tracing::error!("Searching for position of range = {:?}", range);

            let new_hashes = self.frame_hashes.borrow();

            let mut scroll_target = None;

            // Find position to scroll to
            for (page_index, page) in document.pages.iter().enumerate() {
                // Avoid searching through all pages by checking the hashes.
                if page_index < old_hashes.len() && new_hashes[page_index] == old_hashes[page_index]
                {
                    continue;
                }

                tracing::error!("-> searching in page {}", page_index);

                if let Some(ypos) = Self::find_ypos_from_source_range(page, &range, source) {
                    let page_size = page.size().to_point().y.to_pt() as f32;
                    scroll_target = Some((page_index, page_size, ypos));
                    break;
                }
            }

            // If, for example, a line is deleted, the `range` may not be found in the
            // new document. But it will be in the old document. So look there for a
            // position to scroll to (the position where now something is missing).
            // TODO: deduplicate code
            if scroll_target.is_none() && old_document.is_some() && old_source.is_some() {
                let old_document = old_document.unwrap();
                for (page_index, page) in old_document.pages.iter().enumerate() {
                    // Avoid searching through all pages by checking the hashes.
                    if page_index < new_hashes.len()
                        && new_hashes[page_index] == old_hashes[page_index]
                    {
                        continue;
                    }

                    tracing::error!("-> searching IN OLD page {}", page_index);

                    if let Some(ypos) = Self::find_ypos_from_source_range(
                        page,
                        &range,
                        old_source.as_ref().unwrap(),
                    ) {
                        let page_size = page.size().to_point().y.to_pt() as f32;
                        scroll_target = Some((page_index, page_size, ypos));
                        break;
                    }
                }
            }

            // Scroll to found position
            if let Some((page_index, page_size, ypos)) = scroll_target {
                let zoom = self.zoom.borrow().clone();

                // TODO: sometimes this scrolls to the "correct" location only on the 2nd try/change.
                //       Seems to happen only when scrolling to a different page.
                //       Maybe that's b/c scroll happens before that page is (lazily) loaded?
                //       Maybe: It's just the slint "fail to redraw" bug again in some form?
                self.main_window_weak
                    .upgrade_in_event_loop(move |main_window| {
                        // Take into account zoom
                        // Take into account the factor (1.6666666 * 1phx/1px)
                        let image_scale = zoom * (1.6666666 / main_window.window().scale_factor());

                        // add page offset, take into account zoom
                        // TODO: this assumes all pages have same height.
                        let ypos = (ypos.to_pt() as f32) * image_scale
                            + 5.0
                            + (page_index as f32) * (page_size * image_scale + 10.0);

                        tracing::error!("scrolling to {:?} on page {:?}", ypos, page_index);
                        let current_ypos = main_window.get_list_viewport_y().abs();
                        let current_visible_height = main_window.get_list_visible_height();

                        // Only scroll if `ypos` not not already visible
                        if ypos < current_ypos || ypos > current_ypos + current_visible_height {
                            // Don't put the last change at the very top of the viewport.
                            // Want to see some stuff above last change as well.
                            let ypos = ypos - current_visible_height * 0.3;
                            main_window.set_list_viewport_y(-ypos);
                        }
                    })
                    .unwrap();
            }
        }
    }

    fn overlap(r1: &std::ops::Range<usize>, r2: &std::ops::Range<usize>) -> bool {
        (r1.start <= r2.end) && (r2.start <= r1.end)
    }

    fn find_ypos_from_source_range(
        frame: &typst::layout::Frame,
        range: &std::ops::Range<usize>,
        source: &typst::syntax::Source,
    ) -> Option<typst::layout::Abs> {
        let zero_point = typst::layout::Point::zero();
        for (point, frame_item) in frame.items() {
            match frame_item {
                typst::layout::FrameItem::Text(text_item) => {
                    let glyphs = &text_item.glyphs;
                    let Some(first_range) = source.range(glyphs.first().unwrap().span.0) else {
                        continue;
                    };
                    let Some(last_range) = source.range(glyphs.last().unwrap().span.0) else {
                        continue;
                    };
                    let total_range = (first_range.start)..(last_range.end);

                    if Self::overlap(range, &total_range) {
                        return Some(point.y - text_item.size);
                    }
                }
                typst::layout::FrameItem::Meta(meta, size) => {
                    if size.to_point() == zero_point {
                        continue;
                    }
                    let typst::introspection::Meta::Elem(content) = meta else {
                        continue;
                    };

                    let span = content.span();
                    let Some(range_in_source_file) = source.range(span) else {
                        continue;
                    };

                    if Self::overlap(range, &range_in_source_file) {
                        return Some(point.y);
                    }
                }
                typst::layout::FrameItem::Group(group_item) => {
                    if let Some(ypos) =
                        Self::find_ypos_from_source_range(&group_item.frame, &range, &source)
                    {
                        return Some(ypos + point.y);
                    }
                }
                _ => {}
            };
        }
        None
    }

    pub fn set_zoom(&self, zoom: f32) {
        *self.zoom.borrow_mut() = zoom.abs().max(0.3).min(3.0);
        let len = self.images.borrow().len();
        *self.images.borrow_mut() = std::iter::repeat_with(|| None).take(len).collect();
        self.notify.reset();

        self.slint_workaround_redraw();
    }
}

impl Model for LazyImagesModel {
    type Data = slint::Image;

    fn row_count(&self) -> usize {
        self.images.borrow().len()
    }

    fn row_data(&self, row: usize) -> Option<Self::Data> {
        tracing::error!("getting page {} of doc", row);

        let data = self
            .images
            .borrow_mut()
            .get_mut(row)?
            .get_or_insert_with(|| {
                tracing::error!("-> rendering page {} of doc", row);

                let document = self.document.borrow();
                let document = document.as_ref().unwrap();

                let page = document.pages.get(row).unwrap();
                let zoom = self.zoom.borrow().clone();

                let pixmap = typst_render::render(page, zoom * 3.0, typst::visualize::Color::WHITE);
                let width = pixmap.width();
                let height = pixmap.height();
                let pixel_buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                    &pixmap.take(),
                    width,
                    height,
                );

                slint::Image::from_rgba8_premultiplied(pixel_buffer)
            })
            .clone();

        Some(data)
    }

    fn set_row_data(&self, row: usize, data: Self::Data) {
        if row < self.row_count() {
            self.images.borrow_mut()[row] = Some(data);
            self.notify.row_changed(row);
        }
    }

    fn model_tracker(&self) -> &dyn ModelTracker {
        &self.notify
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

thread_local!(static MAIN_WINDOW: MainWindow = MainWindow::new().unwrap());

thread_local!(static IMAGES_MODEL: std::rc::Rc<LazyImagesModel> = MAIN_WINDOW.with(|main_window| {
        std::rc::Rc::new(LazyImagesModel::new(main_window.as_weak()))
    })
);

pub struct Ui {}

impl Ui {
    pub async fn init() -> Self {
        debug!("I am creating a new thread.");

        // The UI / slint event loop thread
        thread::spawn(|| {
            MAIN_WINDOW.with(|main_window| {
                IMAGES_MODEL.with(|model| {
                    main_window.set_image_sources(slint::ModelRc::new(model.clone()))
                });

                main_window.on_zoom_changed(|zoom| {
                    IMAGES_MODEL.with(|model| {
                        model.set_zoom(zoom);
                    })
                });

                main_window.run().unwrap();
            });
        });

        Ui {}
    }

    pub async fn show_document(
        &self,
        document: Arc<Document>,
        source: typst::syntax::Source,
        first_change_range: Option<Range>,
    ) {
        slint::invoke_from_event_loop(move || {
            IMAGES_MODEL.with(|model| model.set_doc(document, source, first_change_range))
        })
        .unwrap();
    }
}

slint::slint! {
    import { ListView } from "std-widgets.slint";
    export component MainWindow inherits Window {
        in property <[image]> image_sources;
        in-out property <length> list_viewport_y <=> mylist.viewport-y;
        out property <length> list_visible_height <=> mylist.visible-height;

        property<float> zoom: 1.0;
        callback zoom_changed(float);

        forward-focus: my-key-handler;
        my-key-handler := FocusScope {
            key-pressed(event) => {
                if (event.modifiers.control) {
                    if (event.text == "=") {
                        zoom = min(zoom + 0.1, 3.0);
                        zoom-changed(zoom);
                    }
                    if (event.text == "-") {
                        zoom = max(zoom - 0.1, 0.3);
                        zoom-changed(zoom);
                    }
                }
                accept
            }
        }

        mylist := ListView {
            // TODO: Handle link clicks
            for image_source in image_sources : Rectangle {
                // 1/3 for resolution
                width: (image_source.width/3) * 1px * (1.6666666 * 1phx/1px);
                height: (image_source.height/3) * 1px * (1.6666666 * 1phx/1px) + 10px; // +10px for spacing
                x: max(0px, (parent.width - self.width) / 2);
                Image {
                    width: parent.width;
                    source: image_source;
                }
            }
        }
    }
}
