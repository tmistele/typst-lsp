use once_cell::sync::OnceCell;
use send_wrapper::SendWrapper;
use slint::{Model, ModelNotify, ModelTracker};
use std::sync::mpsc::Receiver as StdReceiver;
use std::sync::mpsc::Sender as StdSender;
use std::sync::Arc;
use std::thread;
use std::{cell::RefCell, sync::Mutex};
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio::sync::oneshot;
use tokio::sync::RwLock;
use tower_lsp::lsp_types::Position as LspPosition;
use tower_lsp::lsp_types::{Range, ShowDocumentParams, Url};
use tower_lsp::Client;
use typst::layout::Position as TypstPosition;
use typst::model::Document;
use typst_ide::Jump;

use crate::server::WorldThread;
use crate::workspace::package::PackageId;
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
    main_window: slint::Weak<MainWindow>,
    ui_request_tx: Sender<UiRequest>,
    pixelbuffer_rx: StdReceiver<slint::SharedPixelBuffer<slint::Rgba8Pixel>>,
}

impl LazyImagesModel {
    pub fn new(
        main_window: slint::Weak<MainWindow>,
        ui_request_tx: Sender<UiRequest>,
        pixelbuffer_rx: StdReceiver<slint::SharedPixelBuffer<slint::Rgba8Pixel>>,
    ) -> Self {
        LazyImagesModel {
            images: RefCell::new(Vec::new()),
            notify: Default::default(),
            main_window,
            ui_request_tx,
            pixelbuffer_rx,
        }
    }

    fn slint_workaround_redraw(&self) {
        // TODO: slint bug workaround
        // https://github.com/slint-ui/slint/issues/3125
        // not sure. the bug fix mentioned there doesn't seem to fix it?
        // only the workaround mentioned there:
        self.main_window
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
                self.ui_request_tx
                    .blocking_send(UiRequest::Render(row))
                    .expect("requesting render failed");

                let pixel_buffer = self.pixelbuffer_rx.recv().expect("receiving pixbuf failed");
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

pub struct Ui {
    document: Mutex<Arc<Document>>,
    source_uri: Mutex<Option<Url>>,
    zoom: Mutex<f32>,
    workspace: Arc<OnceCell<Arc<RwLock<Workspace>>>>,
    // TODO: Share a typst thread with the `TypstServer`? Like we share a `Workspace`?
    typst_thread: TypstThread,
    client: Client,
    main_window: slint::Weak<MainWindow>,
    images_model: Arc<SendWrapper<std::rc::Rc<LazyImagesModel>>>,
}

pub struct NewDocumentMessage {
    pub document: Arc<Document>,
    pub source_uri: Url,
    pub first_change_range: Option<Range>,
}

pub enum UiRequest {
    Render(usize),
    JumpFromClick(ListViewClick),
    Zoom(f32),
}

impl Ui {
    pub async fn run(
        workspace: Arc<OnceCell<Arc<RwLock<Workspace>>>>,
        client: Client,
        mut to_ui_rx: Receiver<NewDocumentMessage>,
    ) {
        let (ui_request_tx, mut ui_request_rx) = channel(10);
        let (pixelbuffer_tx, pixelbuffer_rx) = std::sync::mpsc::channel();

        let (tx_window_and_model, rx_window_and_model) = tokio::sync::oneshot::channel();

        // The UI / slint event loop thread
        let jump_click_tx = ui_request_tx.clone();
        let zoom_tx = ui_request_tx.clone();
        thread::spawn(|| {
            let main_window = MainWindow::new().unwrap();
            let images_model = std::rc::Rc::new(LazyImagesModel::new(
                main_window.as_weak(),
                ui_request_tx,
                pixelbuffer_rx,
            ));

            main_window.set_image_sources(slint::ModelRc::from(images_model.clone()));

            main_window.on_zoom_changed(move |zoom| {
                zoom_tx
                    .blocking_send(UiRequest::Zoom(zoom))
                    .expect("could not send zoom request");
            });

            main_window.on_clicked(move |click: ListViewClick| {
                jump_click_tx
                    .blocking_send(UiRequest::JumpFromClick(click))
                    .expect("could not send jump click request");
            });

            let _ =
                tx_window_and_model.send((main_window.as_weak(), SendWrapper::new(images_model)));

            main_window.run().unwrap();
        });

        let (main_window, images_model) = rx_window_and_model.await.unwrap();

        let ui = Self {
            document: Default::default(),
            source_uri: Default::default(),
            zoom: Mutex::new(1.0),
            typst_thread: Default::default(),
            workspace,
            client,
            main_window,
            images_model: Arc::new(images_model),
        };

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

                ui.show_document(msg.document, msg.source_uri, msg.first_change_range)
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
                        let document = ui.document.lock().unwrap().to_owned();

                        let zoom = ui.zoom.lock().unwrap().clone();

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
                    UiRequest::JumpFromClick(click) => {
                        tracing::error!("got ui click! {:?}", click);
                        ui.jump_from_click(click).await;
                    }
                    UiRequest::Zoom(zoom) => {
                        tracing::error!("got zoom request {}", zoom);
                        *ui.zoom.lock().unwrap() = zoom.abs().max(0.3).min(3.0);
                        let number_pages = ui.document.lock().unwrap().pages.len();

                        let model = Arc::clone(&ui.images_model);
                        slint::invoke_from_event_loop(move || {
                            model.reset_all(number_pages);
                        })
                        .unwrap();
                    }
                }
            }
        };
        futures::join!(fut1, fut2);
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

    async fn jump_from_click(&self, click: ListViewClick) {
        // Find the page from which the click came.
        let document = self.document.lock().unwrap();
        let document = document.to_owned();

        let (page_index, page_x, page_y) = {
            let mut page_y = click.listview_y;
            let mut page_x = click.listview_x;
            let mut found_page_index = None;
            let mut ypos = 5.0;
            for (page_index, page) in document.pages.iter().enumerate() {
                page_y = click.listview_y - ypos;
                ypos += (page.height().to_pt() as f32) * click.image_scale;
                tracing::error!(
                    "checking -> checking if in page ending at {} (rel y = {})",
                    ypos,
                    page_y
                );
                if ypos > click.listview_y {
                    let page_width = (page.width().to_pt() as f32) * click.image_scale;
                    let page_position_x = (click.viewport_visible_width - page_width) / 2.0;
                    let page_position_x = page_position_x.max(0.0);
                    page_x = click.listview_x - page_position_x;
                    found_page_index = Some(page_index);
                    break;
                }
                ypos += 10.0;
            }
            let Some(found_page_index) = found_page_index else {
                return;
            };
            (found_page_index, page_x, page_y)
        };
        tracing::error!("-> click relative to page y = {}, x = {}", page_y, page_x);

        // Find jump location from position in that page
        let (tx, rx) = oneshot::channel();
        let document_for_typst = document.clone(); // Keep `document` alive for later
        self.thread_with_world()
            .await
            .run(move |world| {
                // `image_scale` takes into account zoom level etc.
                let point = typst::layout::Point {
                    x: typst::layout::Abs::pt((page_x / click.image_scale).into()),
                    y: typst::layout::Abs::pt((page_y / click.image_scale).into()),
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
            self.position_highlight(click.x, click.y, HighlightMode::Warning);
            self.show_status("Nothing to click here...".into(), HighlightMode::Warning);
            return;
        };

        // Do the jump
        match jump {
            Jump::Source(file_id, position) => {
                let (uri, source) = {
                    let workspace = Arc::clone(self.workspace()).read_owned().await;
                    let package_id = if let Some(package_spec) = file_id.package() {
                        // TODO: Is there a way to avoid the clone?
                        PackageId::new_external(package_spec.clone())
                    } else {
                        workspace
                            .full_id(
                                self.source_uri
                                    .lock()
                                    .unwrap()
                                    .as_ref()
                                    .expect("Do not have a source uri?"),
                            )
                            .unwrap()
                            .package()
                    };

                    let package = workspace
                        .package_manager()
                        .package(package_id)
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

                self.position_highlight(click.x, click.y, HighlightMode::Normal);
                self.client
                    .show_document(params)
                    .await
                    .expect("could not show document?");
            }
            Jump::Position(position) => {
                self.position_highlight(click.x, click.y, HighlightMode::Normal);
                self.scroll(&document, self.zoom.lock().unwrap().clone(), &position);
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
                        self.show_status(
                            format!("Could not parse URL {}", url).into(),
                            HighlightMode::Warning,
                        );
                        return;
                    }
                };

                tracing::error!("-> external URL = {:?}", params);

                self.position_highlight(click.x, click.x, HighlightMode::Normal);
                self.show_status(format!("Opening URL {}", url).into(), HighlightMode::Normal);
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

        let model = Arc::clone(&self.images_model);
        slint::invoke_from_event_loop(move || {
            model.reset_all(new_len);
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
        let main_window = self.main_window.clone();
        tokio::spawn(async move {
            let cursor = source
                .line_column_to_byte(range.start.line as usize, range.start.character as usize)
                .unwrap_or_else(|| source.len_bytes() - 1);
            if let Some(position) = typst_ide::jump_from_cursor(&document, &source, cursor + 1) {
                Self::scroll_in_window(main_window, &document, zoom, &position);
            }
        });
    }

    fn position_highlight(&self, x: f32, y: f32, mode: HighlightMode) {
        self.main_window
            .upgrade_in_event_loop(move |main_window| {
                // TODO: What if a second event comes in? Should just delay the timer
                let main_window_weak = main_window.as_weak();
                slint::Timer::single_shot(std::time::Duration::from_millis(125), move || {
                    main_window_weak
                        .upgrade()
                        .unwrap()
                        .set_position_highlight_visible(false);
                });

                main_window.set_position_highlight(PositionHighlight { x, y, mode });
                main_window.set_position_highlight_visible(true);
            })
            .unwrap();
    }

    fn scroll(&self, document: &Arc<Document>, zoom: f32, position: &TypstPosition) {
        Self::scroll_in_window(self.main_window.clone(), document, zoom, position);
    }

    fn scroll_in_window(
        main_window: slint::Weak<MainWindow>,
        document: &Arc<Document>,
        zoom: f32,
        position: &TypstPosition,
    ) {
        tracing::error!("-> got position to scroll to! {:?}", position);
        // TODO: sometimes this scrolls to the "correct" location only on the 2nd try/change.
        //       see https://github.com/slint-ui/slint/issues/4463
        let page_index = position.page.get() - 1;
        let page_size = document.pages[page_index].size().to_point().y.to_pt() as f32;
        let ypos = position.point.y;

        main_window
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

    fn show_status(&self, text: slint::SharedString, mode: HighlightMode) {
        self.main_window
            .upgrade_in_event_loop(move |main_window| {
                let main_window_weak = main_window.as_weak();
                // TODO: What if another message comes in? Should reset the timer.
                slint::Timer::single_shot(std::time::Duration::from_millis(250), move || {
                    main_window_weak.upgrade().unwrap().set_status(Status {
                        text: "".into(),
                        mode: HighlightMode::Normal,
                    });
                });
                main_window.set_status(Status { text, mode });
            })
            .unwrap();
    }
}

slint::slint! {
    import { ListView } from "std-widgets.slint";

    export enum HighlightMode { normal, warning }
    export struct PositionHighlight {
        x: length,
        y: length,
        mode: HighlightMode,
    }

    export struct ListViewClick {
        x: length,
        y: length,
        listview_x: length,
        listview_y: length,
        image_scale: float,
        viewport_visible_width: length,
    }

    export struct Status {
        text: string,
        mode: HighlightMode,
    }

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

        callback clicked(ListViewClick);
        my-touch-area := TouchArea {
            width: mylist.width;
            height: mylist.height;
            clicked => {
                clicked({
                    x: my-touch-area.pressed-x,
                    y: my-touch-area.pressed-y,
                    // note: viewport offset is negative
                    listview_x: - mylist.viewport-x + my-touch-area.pressed-x,
                    listview_y: - mylist.viewport-y + my-touch-area.pressed-y,
                    image_scale: (1.6666666 * 1phx/1px)*zoom,
                    viewport_visible_width: mylist.visible-width,
               });
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

        in property <Status> status;
        Rectangle {
            height: 20px;
            width: parent.width;
            y: parent.height - self.height;
            background: status.mode == HighlightMode.warning ? rgb(187, 169, 69) : rgb(68, 68, 68);
            visible: status.text != "";
            Text {
                horizontal-alignment: center;
                vertical-alignment: center;
                color: rgb(254, 254, 254);
                font-size: 10px;
                text: status.text;
            }
        }

        in property <PositionHighlight> position_highlight;
        in property <bool> position_highlight_visible: false;
        Rectangle {
            x: position_highlight.x - self.width/2;
            y: position_highlight.y - self.height/2;
            visible: position_highlight_visible;
            width: 15px;
            height: 15px;
            background: @radial-gradient(
                circle,
                (
                    position_highlight.mode == HighlightMode.warning ?
                        rgb(187, 169, 69) :
                        rgb(68, 68, 68)
                ) 0.2 * mod(animation-tick(), 0.3s) / 0.3s,
                white 0.5 * mod(animation-tick(), 0.3s) / 0.3s + 0.4,
                transparent
            );
        }
    }
}
