//! Port of ui/pages/all_moods.py: every pill of one category row as a list,
//! for when the row on Explore had more than it could show.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::prelude::*;

use crate::net::explore::Category;
use crate::ui::context::{NavRequest, UiContext};

pub struct AllMoodsPage {
    root: gtk::Box,
    rows: RefCell<Vec<(gtk::ListBoxRow, String)>>,
}

impl AllMoodsPage {
    pub fn new(ctx: Rc<UiContext>, title: &str, items: Vec<Category>) -> Rc<Self> {
        let root = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
        let scrolled = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).vscrollbar_policy(gtk::PolicyType::Automatic).vexpand(true).build();
        crate::ui::suppress_hover_while_scrolling(&scrolled);

        let content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(16).margin_top(24).margin_bottom(24).margin_start(24).margin_end(24).build();
        content.append(&gtk::Label::builder().label(display_title(title)).css_classes(["title-1"]).halign(gtk::Align::Start).margin_bottom(16).build());

        let list = gtk::ListBox::builder().css_classes(["boxed-list", "songs-list"]).selection_mode(gtk::SelectionMode::None).build();
        let mut rows = Vec::new();
        for item in &items {
            let row = gtk::ListBoxRow::builder().activatable(true).build();
            let inner = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(12).margin_top(12).margin_bottom(12).margin_start(16).margin_end(16).build();
            inner.append(&gtk::Label::builder().label(&item.title).halign(gtk::Align::Start).xalign(0.0).hexpand(true).ellipsize(gtk::pango::EllipsizeMode::End).lines(1).width_chars(1).build());
            inner.append(&gtk::Image::builder().icon_name("go-next-symbolic").valign(gtk::Align::Center).build());
            row.set_child(Some(&inner));
            list.append(&row);
            rows.push((row, item.title.to_lowercase()));
        }
        content.append(&list);

        let clamp = adw::Clamp::builder().maximum_size(1024).tightening_threshold(600).child(&content).build();
        scrolled.set_child(Some(&clamp));
        root.append(&scrolled);

        list.connect_row_activated(move |_, row| {
            if let Some(item) = items.get(row.index().max(0) as usize) {
                ctx.nav.go(NavRequest::Category { title: item.title.clone(), params: item.params.clone() });
            }
        });

        Rc::new(Self { root, rows: RefCell::new(rows) })
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    /// Hide rows whose pill does not contain the query, like filter_content.
    pub fn filter_content(&self, text: &str) {
        let query = text.trim().to_lowercase();
        for (row, title) in self.rows.borrow().iter() {
            row.set_visible(query.is_empty() || title.contains(&query));
        }
    }
}

/// The page and its navigation entry are both named after the row.
pub fn display_title(title: &str) -> String {
    format!("All {title}")
}
