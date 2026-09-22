//! The release notes shown once after an update, with a donation banner. The
//! notes come from the newest `<release>` in the metainfo, which is compiled
//! in so the dialog and the packages always agree on the version. A release
//! with two lists shows the first one, the highlights, and counts the second.

use std::rc::Rc;
use std::sync::LazyLock;

use adw::prelude::*;

use crate::App;
use crate::ui::preferences::{pref_bool, save};
use crate::ui::window::MainWindow;

const METAINFO: &str = include_str!("../../com.pocoguy.Muse.metainfo.xml");
pub const KOFI_URL: &str = "https://ko-fi.com/M8P12091FB";
pub const RELEASES_URL: &str = "https://github.com/m-obeid/Mixtapes/releases";
pub const SPONSORS_URL: &str = "https://github.com/sponsors/m-obeid";
/// Pref keys shared with the onboarding wizard.
pub const SHOW_PREF: &str = "show_release_notes";
pub const SEEN_PREF: &str = "last_seen_version";
pub const DONATION_PREF: &str = "show_donation_prompt";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    pub date: String,
    pub summary: String,
    /// The first list of the description.
    pub items: Vec<String>,
    /// Entries in any further list, mentioned as a count.
    pub more: usize,
}

static LATEST: LazyLock<Option<Release>> = LazyLock::new(|| parse_latest(METAINFO));

/// The version the packages carry, from the metainfo. Falls back to the crate version.
pub fn current_version() -> &'static str {
    LATEST.as_ref().map(|r| r.version.as_str()).unwrap_or(env!("CARGO_PKG_VERSION"))
}

pub fn latest() -> Option<&'static Release> {
    LATEST.as_ref()
}

fn parse_latest(xml: &str) -> Option<Release> {
    let doc = roxmltree::Document::parse(xml).ok()?;
    let release = doc.descendants().find(|n| n.has_tag_name("release"))?;
    let text = |node: roxmltree::Node| node.descendants().filter(|n| n.is_text()).filter_map(|n| n.text()).collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ");
    let description = release.children().find(|n| n.has_tag_name("description"));
    let summary = description.and_then(|d| d.children().find(|n| n.has_tag_name("p"))).map(text).unwrap_or_default();
    let mut lists = description.into_iter().flat_map(|d| d.children().filter(|n| n.has_tag_name("ul")));
    let entries = |list: roxmltree::Node| list.children().filter(|n| n.has_tag_name("li")).map(text).filter(|s| !s.is_empty()).collect::<Vec<_>>();
    let items = lists.next().map(entries).unwrap_or_default();
    let more = lists.map(|l| entries(l).len()).sum();
    Some(Release {
        version: release.attribute("version")?.to_owned(),
        date: release.attribute("date").unwrap_or_default().to_owned(),
        summary,
        items,
        more,
    })
}

/// True when this version has not been shown yet and the user still wants the notes.
pub fn due(ctx: &App) -> bool {
    let prefs = ctx.paths.read_prefs();
    let shown = prefs.get(SHOW_PREF).and_then(|v| v.as_bool()).unwrap_or(true);
    let seen = prefs.get(SEEN_PREF).and_then(|v| v.as_str()).unwrap_or("");
    shown && seen != current_version()
}

pub fn mark_seen(ctx: &App) {
    save(ctx, SEEN_PREF, current_version());
}

/// Open a web page from a dialog or the window.
pub fn open_url(parent: &impl IsA<gtk::Widget>, url: &str) {
    let window = parent.root().and_downcast::<gtk::Window>();
    gtk::UriLauncher::new(url).launch(window.as_ref(), None::<&gtk::gio::Cancellable>, |_| {});
}

/// Ko-fi and GitHub Sponsors buttons, for the banner and the wizard.
pub fn donate_buttons() -> adw::WrapBox {
    // A wrap box, so the two pills stack on a phone instead of widening the dialog.
    let row = adw::WrapBox::builder().child_spacing(12).line_spacing(12).align(0.5).halign(gtk::Align::Center).build();
    for (label, icon, url) in [("Ko-fi", "ko-fi-symbolic", KOFI_URL), ("GitHub Sponsors", "github-symbolic", SPONSORS_URL)] {
        let button = gtk::Button::builder().css_classes(["pill"]).build();
        button.set_child(Some(&adw::ButtonContent::builder().label(label).icon_name(icon).build()));
        button.connect_clicked(move |b| open_url(b, url));
        row.append(&button);
    }
    row
}

pub fn present(win: &Rc<MainWindow>, ctx: &Rc<App>) -> Option<adw::Dialog> {
    let release = latest()?;
    mark_seen(ctx);

    // Notes, laid out like Bazaar's: title, date, the changes, a closing line, one link.
    let column = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(12).margin_top(6).margin_bottom(24).margin_start(18).margin_end(18).build();
    let title = gtk::Label::builder().label(format!("What's New in {}?", release.version)).css_classes(["title-2"]).wrap(true).justify(gtk::Justification::Center).build();
    column.append(&title);
    if !release.date.is_empty() {
        column.append(&gtk::Label::builder().label(format!("Released on {}", release.date)).css_classes(["dim-label"]).margin_bottom(12).build());
    }
    if !release.summary.is_empty() {
        column.append(&gtk::Label::builder().label(&release.summary).wrap(true).xalign(0.0).build());
    }
    if !release.items.is_empty() {
        column.append(&gtk::Label::builder().label("Changes").xalign(0.0).css_classes(["heading"]).build());
        let list = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).margin_start(12).build();
        for item in &release.items {
            let line = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(8).build();
            line.append(&gtk::Label::builder().label("•").valign(gtk::Align::Start).build());
            line.append(&gtk::Label::builder().label(item).wrap(true).xalign(0.0).hexpand(true).build());
            list.append(&line);
        }
        if release.more > 0 {
            list.append(&gtk::Label::builder().label(format!("…and {} more changes and fixes, in the full release notes.", release.more)).wrap(true).xalign(0.0).css_classes(["dim-label"]).margin_top(4).build());
        }
        column.append(&list);
    }
    column.append(&gtk::Label::builder().label("Thanks for reading, and have a great day!").wrap(true).xalign(0.0).margin_top(8).build());
    let full = gtk::Button::builder().halign(gtk::Align::Center).margin_top(12).css_classes(["pill"]).build();
    let content = adw::ButtonContent::builder().label("Full Release Notes").icon_name("external-link-symbolic").build();
    full.set_child(Some(&content));
    full.connect_clicked(|b| open_url(b, RELEASES_URL));
    column.append(&full);

    let clamp = adw::Clamp::builder().maximum_size(560).child(&column).build();
    let content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
    content.append(&clamp);
    let viewport = gtk::Viewport::builder().scroll_to_focus(false).child(&content).build();
    let scroller = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).propagate_natural_height(true).vexpand(true).child(&viewport).build();

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::builder().show_title(false).css_classes(["flat"]).build());
    toolbar.set_content(Some(&scroller));
    // Breakpoints want a minimum size on the dialog.
    let dialog = adw::Dialog::builder().title("What's New").content_width(560).content_height(720).width_request(300).height_request(400).child(&toolbar).build();
    if pref_bool(ctx, DONATION_PREF, true) {
        // Pinned under the notes on a desktop. Narrow, it took half the sheet, so there it
        // scrolls with the notes. The dialog's own width decides, so a live resize moves it.
        let banner = donation_banner();
        toolbar.add_bottom_bar(&banner);
        toolbar.set_bottom_bar_style(adw::ToolbarStyle::Flat);
        let narrow = adw::Breakpoint::new(adw::BreakpointCondition::new_length(adw::BreakpointConditionLengthType::MaxWidth, 500.0, adw::LengthUnit::Sp));
        {
            let (toolbar, content, banner) = (toolbar.clone(), content.clone(), banner.clone());
            narrow.connect_apply(move |_| {
                toolbar.remove(&banner);
                content.append(&banner);
            });
        }
        {
            let (toolbar, content, banner) = (toolbar.clone(), content.clone(), banner.clone());
            narrow.connect_unapply(move |_| {
                content.remove(&banner);
                toolbar.add_bottom_bar(&banner);
            });
        }
        dialog.add_breakpoint(narrow);
    }
    dialog.present(Some(win.window()));
    Some(dialog)
}

/// The banner under the notes. Off with the `show_donation_prompt` pref.
fn donation_banner() -> gtk::Box {
    let banner = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(12).css_classes(["donation-banner"]).build();
    banner.append(&gtk::Label::builder().label("This version of Mixtapes was made possible by users like you!").css_classes(["title-2"]).wrap(true).justify(gtk::Justification::Center).build());
    banner.append(&gtk::Label::builder().label("I love making Mixtapes and my other projects, but I can't do it without help. Support my work with a donation:").wrap(true).justify(gtk::Justification::Center).build());
    let buttons = donate_buttons();
    buttons.set_margin_top(6);
    banner.append(&buttons);
    banner
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_newest_release_is_parsed_with_its_bullets() {
        let xml = r#"<component><releases>
            <release version="2026-09-12.0" date="2026-09-12"><description>
              <p>Small   screens.</p><ul><li>One <em>thing</em></li><li>Two</li></ul>
              <ul><li>Fix a</li><li>Fix b</li><li>Fix c</li></ul>
            </description></release>
            <release version="2026-09-04.0" date="2026-09-04"><description><p>Older</p></description></release>
        </releases></component>"#;
        let release = parse_latest(xml).unwrap();
        assert_eq!(release.version, "2026-09-12.0");
        assert_eq!(release.date, "2026-09-12");
        assert_eq!(release.summary, "Small screens.");
        assert_eq!(release.items, vec!["One thing", "Two"]);
        assert_eq!(release.more, 3);
    }

    #[test]
    fn the_shipped_metainfo_has_a_release() {
        let release = latest().expect("metainfo release");
        assert!(!release.version.is_empty());
        assert!(!release.items.is_empty());
        assert_eq!(current_version(), release.version);
    }
}
