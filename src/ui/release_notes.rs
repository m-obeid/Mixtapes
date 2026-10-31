//! The release notes shown once after an update, with a donation banner. The
//! notes come from the newest `<release>` in the metainfo, which is compiled
//! in so the dialog and the packages always agree on the version. A release
//! with two lists shows both in full: what changed, then what was fixed.

use std::rc::Rc;
use std::sync::LazyLock;

use adw::prelude::*;

use crate::App;
use crate::ui::preferences::{pref_bool, save};
use crate::ui::window::MainWindow;

const METAINFO: &str = include_str!("../../com.pocoguy.Muse.metainfo.xml");
pub const KOFI_URL: &str = "https://ko-fi.com/M8P12091FB";
pub const SPONSORS_URL: &str = "https://github.com/sponsors/m-obeid";
/// Pref keys shared with the onboarding wizard.
pub const SHOW_PREF: &str = "show_release_notes";
pub const SEEN_PREF: &str = "last_seen_version";
pub const DONATION_PREF: &str = "show_donation_prompt";
/// The widest the notes and the banner under them lay out their text.
const NOTES_WIDTH: i32 = 560;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    pub date: String,
    pub summary: String,
    /// The first list of the description.
    pub items: Vec<String>,
    /// The entries of any further list, shown under their own heading.
    pub fixes: Vec<String>,
    /// Where "Full Release Notes" goes when not this version's own page, for a hotfix of a bigger release.
    pub details_url: Option<String>,
}

/// Each version's GitHub release, with its full changelog.
const RELEASES_URL: &str = "https://github.com/m-obeid/Mixtapes/releases/tag";

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
    let fixes = lists.flat_map(entries).collect();
    let details_url = release.children().find(|n| n.has_tag_name("url") && n.attribute("type") == Some("details")).and_then(|n| n.text()).map(|url| url.trim().to_owned());
    Some(Release {
        version: release.attribute("version")?.to_owned(),
        date: release.attribute("date").unwrap_or_default().to_owned(),
        summary,
        items,
        fixes,
        details_url,
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

    // Notes, laid out like Bazaar's: title, date, the changes, the fixes, a closing line.
    let column = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(12).margin_top(6).margin_bottom(24).margin_start(18).margin_end(18).build();
    let title = gtk::Label::builder().label(tr!("What's New in {version}?", version = release.version)).css_classes(["title-2"]).wrap(true).justify(gtk::Justification::Center).build();
    column.append(&title);
    if !release.date.is_empty() {
        column.append(&gtk::Label::builder().label(tr!("Released on {date}", date = release.date)).css_classes(["dim-label"]).margin_bottom(12).build());
    }
    if !release.summary.is_empty() {
        column.append(&gtk::Label::builder().label(&release.summary).wrap(true).xalign(0.0).build());
    }
    // The metainfo holds the short version. The GitHub release has the rest.
    for (heading, entries) in [(tr!("Changes"), &release.items), (tr!("Fixes"), &release.fixes)] {
        if entries.is_empty() {
            continue;
        }
        column.append(&gtk::Label::builder().label(heading).xalign(0.0).css_classes(["heading"]).build());
        let list = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).margin_start(12).build();
        for item in entries {
            let line = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(8).build();
            line.append(&gtk::Label::builder().label("•").valign(gtk::Align::Start).build());
            line.append(&gtk::Label::builder().label(item).wrap(true).xalign(0.0).hexpand(true).build());
            list.append(&line);
        }
        column.append(&list);
    }
    column.append(&gtk::Label::builder().label(tr!("Thanks for reading, and have a great day!")).wrap(true).xalign(0.0).margin_top(8).build());
    let full = gtk::Button::builder().halign(gtk::Align::Center).margin_top(12).css_classes(["pill"]).build();
    full.set_child(Some(&adw::ButtonContent::builder().label(tr!("Full Release Notes")).icon_name("external-link-symbolic").build()));
    let address = release.details_url.clone().unwrap_or_else(|| format!("{RELEASES_URL}/{}", release.version));
    full.connect_clicked(move |button| open_url(button, &address));
    column.append(&full);

    let clamp = adw::Clamp::builder().maximum_size(NOTES_WIDTH).child(&column).build();
    let content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
    content.append(&clamp);
    let viewport = gtk::Viewport::builder().scroll_to_focus(false).child(&content).build();
    let scroller = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).propagate_natural_height(true).vexpand(true).child(&viewport).build();

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::builder().show_title(false).css_classes(["flat"]).build());
    toolbar.set_content(Some(&scroller));
    // Breakpoints want a minimum size on the dialog.
    let dialog = adw::Dialog::builder().title(tr!("What's New")).content_width(560).width_request(300).height_request(400).child(&toolbar).build();
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
    let banner = gtk::Box::builder().orientation(gtk::Orientation::Vertical).css_classes(["donation-banner"]).build();
    let column = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(12).build();
    column.append(&gtk::Label::builder().label(tr!("This version of Mixtapes was made possible by users like you!")).css_classes(["title-2"]).wrap(true).justify(gtk::Justification::Center).build());
    column.append(&gtk::Label::builder().label(tr!("I love making Mixtapes and my other projects, but I can't do it without help. Support my work with a donation:")).wrap(true).justify(gtk::Justification::Center).build());
    let buttons = donate_buttons();
    buttons.set_margin_top(6);
    column.append(&buttons);
    // The same clamp as the notes, so the text of both starts and ends on one line.
    banner.append(&adw::Clamp::builder().maximum_size(NOTES_WIDTH).child(&column).build());
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
        assert_eq!(release.fixes, vec!["Fix a", "Fix b", "Fix c"]);
        assert_eq!(release.details_url, None);
    }

    #[test]
    fn a_hotfix_can_point_at_the_release_it_fixes() {
        let xml = r#"<component><releases><release version="1.1"><url type="details"> https://example.org/1.0 </url></release></releases></component>"#;
        assert_eq!(parse_latest(xml).unwrap().details_url.as_deref(), Some("https://example.org/1.0"));
    }

    #[test]
    fn the_shipped_metainfo_has_a_release() {
        let release = latest().expect("metainfo release");
        assert!(!release.version.is_empty());
        assert!(!release.summary.is_empty() || !release.items.is_empty());
        assert_eq!(current_version(), release.version);
    }
}
