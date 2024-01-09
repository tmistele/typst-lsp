use slint::{Model, ModelNotify, ModelTracker};
use std::cell::RefCell;
use std::sync::Arc;
use std::thread;
use tokio::sync::mpsc;
use tracing::debug;
use typst::model::Document;

// TODO: why do we panic when closing the window??
//       -> If I comment out the tracing_subscriber::registery().init() thing the crash goes away
//       (in src/logging.rs)

// Model that lazily converts pages of a typst `Document` to a `slint::image` when they are scrolled into view.
// The usefulness of this comes from slint's `ListView` only instantiating elements that are visible.
pub struct LazyImagesModel {
    images: RefCell<Vec<Option<slint::Image>>>,
    source_document: RefCell<Option<Arc<Document>>>,
    notify: ModelNotify,
}

impl LazyImagesModel {
    pub fn set_doc(&self, new: Arc<Document>) {
        let len = new.pages.len();
        *self.source_document.borrow_mut() = Some(new);
        *self.images.borrow_mut() = std::iter::repeat_with(|| None).take(len).collect();
        self.notify.reset();
    }

    pub fn new(doc: Option<Arc<Document>>) -> Self {
        let len = doc.as_ref().map(|x| x.pages.len()).unwrap_or(0);
        LazyImagesModel {
            images: RefCell::new(std::iter::repeat_with(|| None).take(len).collect()),
            source_document: RefCell::new(doc),
            notify: Default::default(),
        }
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

                let source_document = self.source_document.borrow();
                let source_document = source_document.as_ref().unwrap();

                let page = source_document.pages.get(row).unwrap();

                let pixmap = typst_render::render(page, 3.0, typst::visualize::Color::WHITE);
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
            tracing::error!("UPDATING ROW DATA");
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

thread_local!(static IMAGES_MODEL: std::rc::Rc<LazyImagesModel> = {
    std::rc::Rc::new(LazyImagesModel::new(None))
});

pub struct Ui {
    sender: mpsc::Sender<Arc<Document>>,
}

impl Ui {
    pub async fn init() -> Self {
        debug!("I am creating a new thread.");

        // TODO: At the moment we don't need this whole main_window_weak thing?
        //       We could use slint::invoke_from_event_loop w/o a handle to the main window?
        let main_window_weak = {
            let (tx, rx) = tokio::sync::oneshot::channel();

            // The UI / slint event loop thread
            thread::spawn(|| {
                let main_window = MainWindow::new().unwrap();

                IMAGES_MODEL.with(|model| {
                    main_window.set_image_sources(slint::ModelRc::new(model.clone()))
                });

                if let Err(_) = tx.send(main_window.as_weak()) {
                    // TODO: error handling?
                } else {
                    main_window.run().unwrap();
                    debug!("done / window closed!!");
                }
            });
            rx.await.unwrap()
        };

        // Render pixmaps from typst to ui
        let (sender, mut receiver) = mpsc::channel::<Arc<Document>>(15);
        tokio::spawn(async move {
            while let Some(document) = receiver.recv().await {
                debug!("got document?");
                Self::render_document(document, &main_window_weak);
            }
        });

        Ui { sender }
    }

    fn render_document(document: Arc<Document>, main_window_weak: &slint::Weak<MainWindow>) {
        main_window_weak
            .upgrade_in_event_loop(move |_main_window| {
                IMAGES_MODEL.with(|model| model.set_doc(document));
            })
            .unwrap();
    }

    pub async fn show_document(&self, document: Arc<Document>) {
        self.sender.send(document).await.unwrap();
    }
}

slint::slint! {
    import { ListView } from "std-widgets.slint";
    export component MainWindow inherits Window {
        in property <[image]> image_sources;
        ListView {
            // TODO: spacing?
            // TODO: zoom?
            for image_source[i] in image_sources : Image {
                source: image_source;
                width: (image_source.width/3) * 1px;
                x: max(0px, (parent.width - self.width) / 2);
            }
        }
    }
}
