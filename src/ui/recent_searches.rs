//! The last searches, offered under the empty search field.

use std::rc::Rc;

use gtk::prelude::*;
use serde_json::Value;

use crate::paths::Paths;

const PREF: &str = "recent_searches";
const KEEP: usize = 7;

/// Put a search on top. A search that grew out of an earlier one replaces it,
/// so typing slowly leaves "blinding lights" and not its first half too.
fn remembered(mut list: Vec<String>, query: &str) -> Vec<String> {
    let lowered = query.to_lowercase();
    list.retain(|old| {
        let old = old.to_lowercase();
        old != lowered && !lowered.starts_with(&old)
    });
    list.insert(0, query.to_owned());
    list.truncate(KEEP);
    list
}

pub struct RecentSearches {
    paths: Paths,
    popover: gtk::Popover,
    list: gtk::ListBox,
    on_pick: Rc<dyn Fn(&str)>,
}

impl RecentSearches {
    pub fn new(paths: Paths, entry: &gtk::SearchEntry, on_pick: impl Fn(&str) + 'static) -> Rc<Self> {
        // Not autohide: that takes the keyboard, and the field has to keep it.
        let popover = gtk::Popover::builder().autohide(false).has_arrow(false).position(gtk::PositionType::Bottom).build();
        popover.set_parent(entry);
        let list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).css_classes(["menu"]).build();
        popover.set_child(Some(&list));
        Rc::new(Self { paths, popover, list, on_pick: Rc::new(on_pick) })
    }

    fn saved(&self) -> Vec<String> {
        self.paths.read_prefs().get(PREF).and_then(Value::as_array).map(|list| list.iter().filter_map(Value::as_str).map(str::to_owned).collect()).unwrap_or_default()
    }

    fn store(&self, list: &[String]) {
        self.paths.update_prefs(|prefs| {
            prefs.insert(PREF.to_owned(), serde_json::json!(list));
        });
    }

    pub fn remember(&self, query: &str) {
        let query = query.trim();
        if !query.is_empty() {
            self.store(&remembered(self.saved(), query));
        }
    }

    pub fn hide(&self) {
        self.popover.popdown();
    }

    /// Show the list, when there is one.
    pub fn show(self: &Rc<Self>) {
        while let Some(row) = self.list.first_child() {
            self.list.remove(&row);
        }
        let saved = self.saved();
        if saved.is_empty() {
            self.hide();
            return;
        }
        for query in saved {
            let content = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(8).build();
            content.append(&gtk::Image::from_icon_name("system-search-symbolic"));
            content.append(&gtk::Label::builder().label(&query).ellipsize(gtk::pango::EllipsizeMode::End).max_width_chars(32).halign(gtk::Align::Start).hexpand(true).build());
            let button = gtk::Button::builder().child(&content).css_classes(["flat"]).build();
            let pick = self.on_pick.clone();
            button.connect_clicked(move |_| pick(&query));
            self.list.append(&button);
        }
        let clear = gtk::Button::builder().label(tr!("Clear Recent Searches")).css_classes(["flat", "dim-label"]).build();
        let weak = Rc::downgrade(self);
        clear.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.store(&[]);
                this.hide();
            }
        });
        self.list.append(&clear);
        self.popover.popup();
    }
}

#[cfg(test)]
mod tests {
    use super::remembered;

    fn list(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn the_newest_search_leads_and_a_repeat_moves_up() {
        assert_eq!(remembered(list(&["a", "b"]), "b"), ["b", "a"]);
        assert_eq!(remembered(list(&["Queen"]), "queen"), ["queen"]);
    }

    #[test]
    fn a_search_that_grew_replaces_its_beginning() {
        assert_eq!(remembered(list(&["blind", "queen"]), "blinding lights"), ["blinding lights", "queen"]);
    }

    #[test]
    fn only_the_last_few_stay() {
        let long: Vec<String> = (0..7).map(|i| format!("q{i}")).collect();
        let kept = remembered(long, "new");
        assert_eq!((kept.len(), kept[0].as_str(), kept[6].as_str()), (7, "new", "q5"));
    }
}
