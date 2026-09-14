//! Large cover art as a gtk::Picture with cover fit, loaded through the
//! shared texture loader. Port of the parts of AsyncPicture the player views use.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::{gdk, glib};

use crate::net::NetHandle;
use crate::ui::cover::load_texture;

pub struct CoverPicture {
    picture: gtk::Picture,
    net: NetHandle,
    current: RefCell<Option<String>>,
}

impl CoverPicture {
    pub fn new(net: NetHandle) -> Rc<Self> {
        let picture = gtk::Picture::builder().content_fit(gtk::ContentFit::Cover).can_shrink(true).build();
        Rc::new(Self { picture, net, current: RefCell::new(None) })
    }

    pub fn widget(&self) -> &gtk::Picture {
        &self.picture
    }

    #[allow(dead_code)]
    pub fn url(&self) -> Option<String> {
        self.current.borrow().clone()
    }

    pub fn load(self: &Rc<Self>, url: &str) {
        if url.is_empty() {
            self.current.replace(None);
            self.picture.set_paintable(gdk::Paintable::NONE);
            return;
        }
        let url = url.to_owned();
        if self.current.borrow().as_deref() == Some(url.as_str()) {
            return;
        }
        self.current.replace(Some(url.clone()));
        let net = self.net.clone();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            // None: this one is drawn as large as the window allows.
            let texture = load_texture(&net, &url, None).await;
            let Some(this) = weak.upgrade() else { return };
            if this.current.borrow().as_deref() != Some(url.as_str()) {
                return;
            }
            this.picture.set_paintable(texture.as_ref());
        });
    }
}
