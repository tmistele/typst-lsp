use once_cell::sync::OnceCell;
use slint::{Model, ModelNotify, ModelTracker};
use std::sync::mpsc::Receiver as StdReceiver;
use std::sync::mpsc::Sender as StdSender;
use std::sync::Arc;
use std::thread;
use std::{cell::RefCell, sync::Mutex};
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio::sync::oneshot;
use tokio::sync::RwLock;
use tower_lsp::lsp_types::MessageType;
use tower_lsp::lsp_types::Position as LspPosition;
use tower_lsp::lsp_types::{Range, ShowDocumentParams, Url};
use tower_lsp::Client;
use typst::layout::Position as TypstPosition;
use typst::model::Document;
use typst_ide::Jump;

use crate::server::WorldThread;
use crate::workspace::project::Project;
use crate::workspace::world::typst_thread::TypstThread;
use crate::workspace::Workspace;

// TODO: why do we panic when closing the window??
//       -> If I comment out the tracing_subscriber::registery().init() thing the crash goes away
//       (in src/logging.rs)

// Model that lazily converts pages of a typst `Document` to a `slint::image` when they are scrolled into view.
// The usefulness of this comes from slint's `ListView` only instantiating elements that are visible.
pub struct LazyImagesModel {
    images: RefCell<Vec<Option<slint::Image>>>,
    notify: ModelNotify,
    main_window_weak: slint::Weak<MainWindow>,
    ui_request_tx: RefCell<Option<Sender<UiRequest>>>,
    pixelbuffer_rx: RefCell<Option<StdReceiver<slint::SharedPixelBuffer<slint::Rgba8Pixel>>>>,
}

impl LazyImagesModel {
    pub fn new(main_window_weak: slint::Weak<MainWindow>) -> Self {
        LazyImagesModel {
            images: RefCell::new(Vec::new()),
            notify: Default::default(),
            main_window_weak,
            ui_request_tx: RefCell::new(None),
            pixelbuffer_rx: RefCell::new(None),
        }
    }

    pub fn set_rxtx(
        &self,
        ui_request_tx: Sender<UiRequest>,
        pixelbuffer_rx: StdReceiver<slint::SharedPixelBuffer<slint::Rgba8Pixel>>,
    ) {
        *self.ui_request_tx.borrow_mut() = Some(ui_request_tx);
        *self.pixelbuffer_rx.borrow_mut() = Some(pixelbuffer_rx);
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

    pub fn reset_all(&self, new_len: usize) {
        *self.images.borrow_mut() = std::iter::repeat_with(|| None).take(new_len).collect();
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
                let tx = self.ui_request_tx.borrow();
                let tx = tx.as_ref().unwrap();
                tx.blocking_send(UiRequest::Render(row))
                    .expect("requesting render failed");

                let rx = self.pixelbuffer_rx.borrow();
                let rx = rx.as_ref().unwrap();

                let pixel_buffer = rx.recv().expect("receiving pixbuf failed");
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

pub struct Ui {
    document: Mutex<Arc<Document>>,
    source_uri: Mutex<Option<Url>>,
    zoom: Mutex<f32>,
    workspace: Arc<OnceCell<Arc<RwLock<Workspace>>>>,
    // TODO: Share a typst thread with the `TypstServer`? Like we share a `Workspace`?
    typst_thread: TypstThread,
    client: Client,
}

pub struct NewDocumentMessage {
    pub document: Arc<Document>,
    pub source_uri: Url,
    pub first_change_range: Option<Range>,
}

pub enum UiRequest {
    Render(usize),
    JumpFromClick(f32, f32, f32, f32),
    Zoom(f32),
}

impl Ui {
    pub fn new(workspace: Arc<OnceCell<Arc<RwLock<Workspace>>>>, client: Client) -> Self {
        Self {
            document: Default::default(),
            source_uri: Default::default(),
            zoom: Mutex::new(1.0),
            typst_thread: Default::default(),
            workspace,
            client,
        }
    }

    fn workspace(&self) -> &Arc<RwLock<Workspace>> {
        self.workspace
            .get()
            .expect("workspace should be initialized")
    }

    async fn thread_with_world(&self) -> WorldThread {
        let (main, main_project) = {
            let uri = self.source_uri.lock().unwrap();
            let uri = uri.as_ref().expect("Do not have a source uri");
            let workspace = Arc::clone(self.workspace()).read_owned().await;
            let full_id = workspace.full_id(&uri).unwrap();
            let source = workspace.read_source(&uri).unwrap();
            let project = Project::new(full_id.package(), workspace);
            (source, project)
        };

        WorldThread {
            main,
            main_project,
            typst_thread: &self.typst_thread,
        }
    }

    pub async fn run(&self, mut to_ui_rx: Receiver<NewDocumentMessage>) {
        let (ui_request_tx, mut ui_request_rx) = channel(10);
        let (pixelbuffer_tx, pixelbuffer_rx) = std::sync::mpsc::channel();

        // The UI / slint event loop thread
        let jump_click_tx = ui_request_tx.clone();
        let zoom_tx = ui_request_tx.clone();
        thread::spawn(|| {
            IMAGES_MODEL.with(|model| model.set_rxtx(ui_request_tx, pixelbuffer_rx));
            MAIN_WINDOW.with(|main_window| {
                IMAGES_MODEL.with(|model| {
                    main_window.set_image_sources(slint::ModelRc::new(model.clone()))
                });

                main_window.on_zoom_changed(move |zoom| {
                    zoom_tx
                        .blocking_send(UiRequest::Zoom(zoom))
                        .expect("could not send zoom request");
                });

                main_window.on_clicked(move |x, y, image_scale, viewport_visible_width| {
                    jump_click_tx
                        .blocking_send(UiRequest::JumpFromClick(
                            x,
                            y,
                            image_scale,
                            viewport_visible_width,
                        ))
                        .expect("could not send jump click request");
                });

                main_window.run().unwrap();
            });
        });

        // Wait for documents to come in from LSP
        let fut1 = async {
            while let Some(msg) = to_ui_rx.recv().await {
                tracing::error!("ok, got document!");
                let mut msg = msg;
                // Don't waste time rendering old versions.
                while let Ok(next_msg) = to_ui_rx.try_recv() {
                    tracing::error!("actually: skipping ahead, got more document!");
                    msg = next_msg;
                }

                self.show_document(msg.document, msg.source_uri, msg.first_change_range)
                    .await;
            }
        };
        // Wait for render requests to come in from slint UI
        let fut2 = async {
            while let Some(ui_request) = ui_request_rx.recv().await {
                match ui_request {
                    UiRequest::Render(page_index) => {
                        tracing::error!("got render request for pgae {}", page_index);

                        // Don't hold the lock the whole time, just clone the `Arc` (`to_owned()`)
                        let document = self.document.lock().unwrap().to_owned();

                        let zoom = self.zoom.lock().unwrap().clone();

                        // Rendering can take a while. So spawn in separate task.
                        // This allows everything else here to proceed.
                        // Importantly, receiving documents can proceed!
                        // So if rendering does take long and lots of new documents come
                        // in while rendering, we will have the newest version of the document
                        // received and will as the next step render the newest version (not all
                        // the already outdated intermediate versions that haven't been received
                        // yet).
                        let response_tx = pixelbuffer_tx.clone();
                        tokio::spawn(async move {
                            Self::render_page(document, zoom, page_index, response_tx).await
                        });
                    }
                    UiRequest::JumpFromClick(x, y, image_scale, viewport_visible_width) => {
                        tracing::error!(
                            "got ui click! {} {} {} {}",
                            x,
                            y,
                            image_scale,
                            viewport_visible_width
                        );
                        self.jump_from_click(x, y, image_scale, viewport_visible_width)
                            .await;
                    }
                    UiRequest::Zoom(zoom) => {
                        tracing::error!("got zoom request {}", zoom);
                        *self.zoom.lock().unwrap() = zoom.abs().max(0.3).min(3.0);
                        let number_pages = self.document.lock().unwrap().pages.len();

                        slint::invoke_from_event_loop(move || {
                            IMAGES_MODEL.with(move |model| model.reset_all(number_pages))
                        })
                        .unwrap();
                    }
                }
            }
        };
        futures::join!(fut1, fut2);
    }

    async fn jump_from_click(&self, x: f32, y: f32, image_scale: f32, viewport_visible_width: f32) {
        // Find the page from which the click came.
        let document = self.document.lock().unwrap();
        let document = document.to_owned();

        let (page_index, x, y) = {
            let mut relative_y = y;
            let mut relative_x = x;
            let mut found_page_index = None;
            let mut ypos = 5.0;
            for (page_index, page) in document.pages.iter().enumerate() {
                relative_y = y - ypos;
                ypos += (page.height().to_pt() as f32) * image_scale;
                tracing::error!(
                    "checking -> checking if in page ending at {} (rel y = {})",
                    ypos,
                    relative_y
                );
                if ypos > y {
                    let page_width = (page.width().to_pt() as f32) * image_scale;
                    let page_x = (viewport_visible_width - page_width) / 2.0;
                    let page_x = page_x.max(0.0);
                    relative_x = x - page_x;
                    found_page_index = Some(page_index);
                    break;
                }
                ypos += 10.0;
            }
            let Some(found_page_index) = found_page_index else {
                return;
            };
            (found_page_index, relative_x, relative_y)
        };
        tracing::error!("-> relative y = {}, x = {}", y, x);

        // Find jump location from position in that page
        let (tx, rx) = oneshot::channel();
        let document_for_typst = document.clone(); // Keep `document` alive for later
        self.thread_with_world()
            .await
            .run(move |world| {
                // `image_scale` takes into account zoom level etc.
                let point = typst::layout::Point {
                    x: typst::layout::Abs::pt((x / image_scale).into()),
                    y: typst::layout::Abs::pt((y / image_scale).into()),
                };
                let jump = typst_ide::jump_from_click(
                    &world,
                    &document_for_typst,
                    &document_for_typst.pages[page_index],
                    point,
                );
                tx.send(jump).expect("couldn't send jump");
            })
            .await;

        let jump = rx.await.expect("couldn't recv jump");
        tracing::error!("-> got jump {:?}", jump);

        let Some(jump) = jump else {
            return;
        };

        // Do the jump
        match jump {
            Jump::Source(file_id, position) => {
                let (uri, source) = {
                    let main_uri = self.source_uri.lock().unwrap();
                    let main_uri = main_uri.as_ref().expect("Do not have a source uri");
                    let workspace = Arc::clone(self.workspace()).read_owned().await;
                    let full_id = workspace.full_id(&main_uri).unwrap();
                    let package = workspace
                        .package_manager()
                        .package(full_id.package())
                        .await
                        .expect("package not found?");
                    let uri = package.vpath_to_uri(file_id.vpath()).unwrap();
                    let source = workspace.read_source(&uri).unwrap();

                    (uri, source)
                };

                let position = LspPosition {
                    line: source
                        .byte_to_line(position)
                        .expect("couldn't map start line") as u32,
                    character: source
                        .byte_to_column(position)
                        .expect("couldn't map start column") as u32,
                };

                tracing::error!("-> jump Source =  {:?}", uri);

                let params = ShowDocumentParams {
                    uri,
                    external: Some(false),
                    take_focus: Some(true),
                    // TODO: does this work with non-ascii?
                    selection: Some(Range {
                        start: position,
                        end: position,
                    }),
                };

                self.client
                    .show_document(params)
                    .await
                    .expect("could not show document?");
            }
            Jump::Position(position) => {
                Self::scroll_ui(&document, self.zoom.lock().unwrap().clone(), &position);
            }
            Jump::Url(url) => {
                let params = if let Ok(url) = Url::parse(url.as_str()) {
                    ShowDocumentParams {
                        uri: url,
                        external: Some(true),
                        take_focus: Some(true),
                        selection: None,
                    }
                } else {
                    let local_url = self
                        .source_uri
                        .lock()
                        .unwrap()
                        .as_ref()
                        .expect("Do not have a source uri")
                        .join(url.as_str());

                    if let Ok(url) = local_url {
                        // Heuristic to open .typ files in same editor
                        let external = Some(!url.as_str().ends_with(".typ"));
                        ShowDocumentParams {
                            uri: url,
                            external,
                            take_focus: Some(true),
                            selection: None,
                        }
                    } else {
                        // TODO: Display some kind of feedback in UI?
                        return;
                    }
                };

                tracing::error!("-> external URL = {:?}", params);

                self.client
                    .show_document(params)
                    .await
                    .expect("could not show document?");
            }
        };
    }

    async fn show_document(
        &self,
        new_doc: Arc<Document>,
        new_source_uri: Url,
        first_change_range: Option<Range>,
    ) {
        let new_len = new_doc.pages.len();

        *self.document.lock().unwrap() = new_doc;
        *self.source_uri.lock().unwrap() = Some(new_source_uri);

        slint::invoke_from_event_loop(move || {
            IMAGES_MODEL.with(move |model| model.reset_all(new_len))
        })
        .unwrap();

        if let Some(range) = first_change_range {
            self.jump_to_first_change(range).await;
        }
    }

    async fn jump_to_first_change(&self, range: Range) {
        // Don't hold the lock the whole time, just clone the `Arc` (`to_owned()`)
        let document = self.document.lock().unwrap().to_owned();
        let zoom = self.zoom.lock().unwrap().clone();

        let source = {
            let main_uri = self.source_uri.lock().unwrap();
            let main_uri = main_uri.as_ref().expect("Do not have a source uri");
            let workspace = Arc::clone(self.workspace()).read_owned().await;
            workspace.read_source(&main_uri).unwrap()
        };

        // Spawn this since this can wait. Make room for new documents to come in as quickly as possible.
        tokio::spawn(async move {
            let cursor = source
                .line_column_to_byte(range.start.line as usize, range.start.character as usize)
                .unwrap_or_else(|| source.len_bytes() - 1);
            if let Some(position) = typst_ide::jump_from_cursor(&document, &source, cursor + 1) {
                Self::scroll_ui(&document, zoom, &position);
            }
        });
    }

    fn scroll_ui(document: &Arc<Document>, zoom: f32, position: &TypstPosition) {
        tracing::error!("-> got position to scroll to! {:?}", position);
        // TODO: sometimes this scrolls to the "correct" location only on the 2nd try/change.
        //       Seems to happen only when scrolling to a different page.
        //       Maybe that's b/c scroll happens before that page is (lazily) loaded?
        //       Maybe: It's just the slint "fail to redraw" bug again in some form?

        let page_index = position.page.get() - 1;
        let page_size = document.pages[page_index].size().to_point().y.to_pt() as f32;
        let ypos = position.point.y;

        slint::invoke_from_event_loop(move || {
            MAIN_WINDOW.with(move |main_window| {
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
            });
        })
        .unwrap();
    }

    async fn render_page(
        document: Arc<Document>,
        zoom: f32,
        page_index: usize,
        pixelbuffer_tx: StdSender<slint::SharedPixelBuffer<slint::Rgba8Pixel>>,
    ) {
        tracing::error!("-> rendering page {} of doc", page_index);
        let page = document.pages.get(page_index).unwrap();

        tracing::error!("-> starting typst_render");
        let pixmap = typst_render::render(page, zoom * 3.0, typst::visualize::Color::WHITE);
        tracing::error!("-> ... done");
        let width = pixmap.width();
        let height = pixmap.height();
        let pixel_buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
            &pixmap.take(),
            width,
            height,
        );

        pixelbuffer_tx
            .send(pixel_buffer)
            .expect("sending pixbuf failed");
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

        callback clicked(float, float, float, float);
        my-touch-area := TouchArea {
            width: mylist.width;
            height: mylist.height;
            clicked => {
                clicked(
                    // note: viewport offset is negative
                    (- mylist.viewport-x + my-touch-area.pressed-x) / 1px,
                    (- mylist.viewport-y + my-touch-area.pressed-y) / 1px,
                    (1.6666666 * 1phx/1px)*zoom,
                    mylist.visible-width / 1px,
               );
            }
        }

        mylist := ListView {
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
