//! Sending files to the uploaded library, and the queue behind the header pie.
//!
//! Port of _start_upload_queue and _process_upload_queue: one row per file with
//! its state, uploads run one after another, and the list clears itself once
//! the last one lands. The library's uploads tab reloads when the queue drains,
//! since that is when the new songs turn up.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use gtk::{gio, glib, prelude::*};

use crate::ui::context::UiContext;

/// Told how far the queue is, so the header pie can follow.
type ProgressSink = Box<dyn Fn(Option<f64>)>;

/// How long finished rows stay up before the popover empties itself.
const LINGER: Duration = Duration::from_secs(8);

struct Row {
    widget: gtk::Box,
    status: gtk::Label,
}

pub struct UploadQueue {
    ctx: Rc<UiContext>,
    items: gtk::Box,
    waiting: RefCell<VecDeque<PathBuf>>,
    rows: RefCell<Vec<(PathBuf, Row)>>,
    running: Cell<bool>,
    done: Cell<usize>,
    total: Cell<usize>,
    clearing: RefCell<Option<glib::SourceId>>,
    on_progress: RefCell<Option<ProgressSink>>,
}

impl UploadQueue {
    pub fn new(ctx: Rc<UiContext>, items: gtk::Box) -> Rc<Self> {
        Rc::new(Self {
            ctx,
            items,
            waiting: RefCell::new(VecDeque::new()),
            rows: RefCell::new(Vec::new()),
            running: Cell::new(false),
            done: Cell::new(0),
            total: Cell::new(0),
            clearing: RefCell::new(None),
            on_progress: RefCell::new(None),
        })
    }

    /// Where the header pie listens.
    pub fn set_on_progress(&self, f: impl Fn(Option<f64>) + 'static) {
        self.on_progress.replace(Some(Box::new(f)));
    }

    /// Ask for files and upload what comes back.
    pub fn pick_files(self: &Rc<Self>, parent: &impl IsA<gtk::Widget>) {
        if !self.ctx.net.client().is_authenticated() {
            crate::ui::toast(parent, "Sign in to upload songs");
            return;
        }
        let filter = gtk::FileFilter::new();
        filter.set_name(Some("Audio Files"));
        for extension in crate::net::uploads::SUPPORTED {
            filter.add_pattern(&format!("*.{extension}"));
        }
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        let dialog = gtk::FileDialog::builder().title("Upload Songs").filters(&filters).default_filter(&filter).build();
        let window = parent.as_ref().root().and_downcast::<gtk::Window>();
        let this = self.clone();
        dialog.open_multiple(window.as_ref(), None::<&gio::Cancellable>, move |result| {
            let Ok(files) = result else { return };
            let paths: Vec<PathBuf> = (0..files.n_items()).filter_map(|i| files.item(i).and_downcast::<gio::File>()).filter_map(|f| f.path()).collect();
            this.queue(paths);
        });
    }

    /// Queue files and start working through them.
    pub fn queue(self: &Rc<Self>, files: Vec<PathBuf>) {
        if files.is_empty() {
            return;
        }
        if let Some(id) = self.clearing.borrow_mut().take() {
            id.remove();
        }
        for file in files {
            self.add_row(&file);
            self.waiting.borrow_mut().push_back(file);
            self.total.set(self.total.get() + 1);
        }
        self.report();
        self.start();
    }

    fn add_row(&self, file: &Path) {
        let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let widget = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).margin_top(4).margin_bottom(4).build();
        widget.append(&gtk::Label::builder().label(&name).halign(gtk::Align::Start).ellipsize(gtk::pango::EllipsizeMode::End).css_classes(["caption"]).build());
        let status = gtk::Label::builder().label("Waiting").halign(gtk::Align::Start).css_classes(["caption", "dim-label"]).build();
        widget.append(&status);
        self.items.append(&widget);
        self.rows.borrow_mut().push((file.to_path_buf(), Row { widget, status }));
    }

    fn set_status(&self, file: &Path, text: &str) {
        if let Some((_, row)) = self.rows.borrow().iter().find(|(path, _)| path == file) {
            row.status.set_label(text);
        }
    }

    fn report(&self) {
        let (done, total) = (self.done.get(), self.total.get());
        if let Some(f) = self.on_progress.borrow().as_ref() {
            f(Some(done as f64 / total.max(1) as f64));
        }
    }

    fn start(self: &Rc<Self>) {
        if self.running.replace(true) {
            return;
        }
        let this = self.clone();
        glib::spawn_future_local(async move { this.work().await });
    }

    /// One file at a time, the way the Python queue worked: YouTube rejects
    /// parallel uploads from one session often enough not to try.
    async fn work(self: Rc<Self>) {
        loop {
            let Some(file) = self.waiting.borrow_mut().pop_front() else { break };
            self.set_status(&file, "Uploading...");
            let http = self.ctx.net.client().http().clone();
            let headers = self.ctx.net.client().browser_headers();
            let path = file.clone();
            let handle = self.ctx.net.spawn(async move {
                match headers {
                    Some(headers) => crate::net::uploads::upload_song(&http, &headers, &path).await,
                    None => Err(crate::net::ytmusic::NetError::Unauthenticated),
                }
            });
            match handle.await {
                Ok(Ok(())) => {
                    tracing::info!(file = %file.display(), "song uploaded");
                    self.set_status(&file, "Uploaded");
                }
                Ok(Err(err)) => {
                    tracing::warn!(%err, file = %file.display(), "upload failed");
                    self.set_status(&file, &err.to_string().chars().take(60).collect::<String>());
                }
                Err(_) => return,
            }
            self.done.set(self.done.get() + 1);
            self.report();
        }
        self.running.set(false);
        self.done.set(0);
        self.total.set(0);
        if let Some(f) = self.on_progress.borrow().as_ref() {
            f(None);
        }
        // YouTube takes a moment to list what was just sent.
        self.ctx.nav.refresh_library();
        self.clear_later();
    }

    fn clear_later(self: &Rc<Self>) {
        if self.clearing.borrow().is_some() {
            return;
        }
        let this = self.clone();
        let id = glib::timeout_add_local_once(LINGER, move || {
            this.clearing.borrow_mut().take();
            for (_, row) in this.rows.borrow_mut().drain(..) {
                this.items.remove(&row.widget);
            }
        });
        self.clearing.replace(Some(id));
    }
}
