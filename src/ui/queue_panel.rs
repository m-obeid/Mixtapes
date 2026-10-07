//! Queue sidebar. Port of ui/queue_panel.py: header with the queue source,
//! count and total time, a bottom bar with shuffle, repeat, add to playlist
//! and clear, drag-and-drop reordering, right-click menu, and a pause-aware
//! playing indicator. Rows follow `QueueEntry` properties
//! through expression bindings, so recycling needs no unbind code.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, glib};

use crate::model::RepeatMode;
use crate::player::Player;
use crate::state::QueueEntry;
use crate::ui::context::UiContext;
use crate::ui::marquee::MarqueeLabel;
use crate::ui::cover::CoverImage;
use crate::ui::context_menu::{MenuAction, Section, SongMenuOptions, show_song_menu};
use crate::ui::widgets::add_to_playlist::AddToPlaylistPopover;

/// Side of a row's cover, a little under the 40px of a playlist row: the sidebar is narrow.
const COVER_SIZE: i32 = 36;

pub struct QueuePanel {
    root: gtk::Box,
    pub header_bar: adw::HeaderBar,
    list: gtk::GridView,
    selection: gtk::NoSelection,
    player: Rc<Player>,
    ctx: Rc<UiContext>,
    scrolled: gtk::ScrolledWindow,
    /// The round arrow button, up while the playing row is scrolled out of sight.
    jump_revealer: gtk::Revealer,
    jump_icon: gtk::Image,
    /// The list keeps the playing row in the middle. Off once the listener scrolls
    /// it out of sight, back on when it is in sight again.
    following: Cell<bool>,
    /// The eased scroll to the playing row, while it runs.
    centring: RefCell<Option<adw::TimedAnimation>>,
}

impl QueuePanel {
    pub fn new(ctx: Rc<UiContext>) -> Rc<Self> {
        let player = ctx.player.clone();
        let state = player.state().clone();
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .css_classes(["background", "queue-panel"])
            .width_request(200)
            .build();

        // -- header ------------------------------------------------------
        let header_bar = adw::HeaderBar::builder()
            .css_classes(["flat"])
            .show_start_title_buttons(false)
            .show_end_title_buttons(false)
            .build();
        // Left aligned. No vertical margin: the header stays as tall as the window's own.
        let title_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .valign(gtk::Align::Center)
            .hexpand(true)
            .margin_start(4)
            .margin_end(4)
            .build();
        // The playlist, album or radio the queue came from. A loose set has no title line.
        let title = MarqueeLabel::new();
        title.add_css_class("sidebar-title");
        {
            let title = title.clone();
            let sync = move |source: String| {
                title.widget().set_visible(!source.is_empty());
                title.set_label(&source);
            };
            sync(state.queue_source_title());
            state.connect_notify_local(Some("queue-source-title"), move |s, _| sync(s.queue_source_title()));
        }
        let count = gtk::Label::builder()
            .css_classes(["caption", "dim-label", "queue-summary"])
            .halign(gtk::Align::Start)
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        {
            let count = count.clone();
            let sync = move |state: &crate::state::PlayerState| {
                count.set_label(&queue_summary(state.queue_length(), state.queue_duration()))
            };
            sync(&state);
            let on_length = sync.clone();
            state.connect_notify_local(Some("queue-length"), move |s, _| on_length(s));
            state.connect_notify_local(Some("queue-duration"), move |s, _| sync(s));
        }
        title_box.append(title.widget());
        title_box.append(&count);
        header_bar.set_title_widget(Some(&title_box));

        let shuffle_btn = gtk::ToggleButton::builder()
            .icon_name("media-playlist-shuffle-symbolic")
            .tooltip_text(tr!("Shuffle Queue"))
            .build();
        state
            .bind_property("shuffle", &shuffle_btn, "active")
            .sync_create()
            .build();
        // Toggle the class like _update_shuffle_state: replacing css-classes wholesale
        // strips the button styles the header bar gave it.
        {
            let btn = shuffle_btn.clone();
            let sync = move |on: bool| {
                if on {
                    btn.add_css_class("accent")
                } else {
                    btn.remove_css_class("accent")
                }
            };
            sync(state.shuffle());
            state.connect_notify_local(Some("shuffle"), move |s, _| sync(s.shuffle()));
        }

        let repeat_btn = gtk::Button::builder()
            .icon_name("media-playlist-consecutive-symbolic")
            .tooltip_text(tr!("Repeat Mode"))
            .build();
        state
            .bind_property("repeat", &repeat_btn, "icon-name")
            .transform_to(|_, mode: RepeatMode| {
                Some(match mode {
                    RepeatMode::Track => "media-playlist-repeat-song-symbolic",
                    RepeatMode::All => "media-playlist-repeat-symbolic",
                    RepeatMode::Off => "media-playlist-consecutive-symbolic",
                })
            })
            .sync_create()
            .build();
        {
            let btn = repeat_btn.clone();
            let sync = move |mode: RepeatMode| {
                if mode == RepeatMode::Off {
                    btn.remove_css_class("accent")
                } else {
                    btn.add_css_class("accent")
                }
            };
            sync(state.repeat());
            state.connect_notify_local(Some("repeat"), move |s, _| sync(s.repeat()));
        }

        let add_btn = gtk::Button::builder()
            .icon_name("list-add-symbolic")
            .tooltip_text(tr!("Add all to Playlist"))
            .build();
        let clear_btn = gtk::Button::builder()
            .icon_name("edit-clear-all-symbolic")
            .tooltip_text(tr!("Clear Queue"))
            .build();

        // -- bottom bar --------------------------------------------------
        // Every queue action sits here: close to the player bar with a mouse, in reach on a tall phone.
        let actions = gtk::CenterBox::builder().css_classes(["toolbar", "queue-actions"]).build();
        let playback = gtk::Box::builder().spacing(6).build();
        playback.append(&shuffle_btn);
        playback.append(&repeat_btn);
        actions.set_start_widget(Some(&playback));
        let edits = gtk::Box::builder().spacing(6).build();
        edits.append(&add_btn);
        edits.append(&clear_btn);
        actions.set_end_widget(Some(&edits));

        let toolbar = adw::ToolbarView::builder().vexpand(true).build();
        toolbar.add_top_bar(&header_bar);
        toolbar.add_bottom_bar(&actions);
        root.append(&toolbar);

        // -- list --------------------------------------------------------
        let factory = gtk::SignalListItemFactory::new();
        let selection = gtk::NoSelection::new(Some(state.queue_model()));
        // The model is attached while the panel is on screen only. A list that
        // was never allocated builds up to 150 rows on every queue change, and
        // both panels did that inside the click that started playback.
        // One column of a grid keeps about 30 rows alive where a ListView keeps 200.
        let list = gtk::GridView::builder()
            .factory(&factory)
            .min_columns(1)
            .max_columns(1)
            .build();
        list.add_css_class("queue-list");
        let scrolled = gtk::ScrolledWindow::builder()
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .css_classes(["queue-scroller"])
            .child(&list)
            .build();
        crate::ui::suppress_hover_while_scrolling(&scrolled);

        // -- the way back to the playing row ------------------------------
        let jump_icon = gtk::Image::from_icon_name("go-down-symbolic");
        let jump_btn = gtk::Button::builder()
            // The size of the lyrics view's round button, 34px, on the dark overlay surface.
            .css_classes(["circular", "osd", "lyrics-osd-btn"])
            .tooltip_text(tr!("Go to the Playing Song"))
            .child(&jump_icon)
            .build();
        let jump_revealer = gtk::Revealer::builder()
            .transition_type(gtk::RevealerTransitionType::Crossfade)
            .transition_duration(150)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::End)
            .margin_bottom(8)
            .child(&jump_btn)
            .build();
        // Hidden, the revealer keeps its size and would swallow clicks meant for the rows under it.
        jump_revealer.set_can_target(false);
        let list_overlay = gtk::Overlay::builder().child(&scrolled).build();
        list_overlay.add_overlay(&jump_revealer);
        toolbar.set_content(Some(&list_overlay));
        // Off screen the list still held about 150 rows, restyled with every track change.
        crate::ui::style_only_while_shown(&root, &scrolled);

        let panel = Rc::new(Self {
            root,
            header_bar,
            list,
            selection,
            player: player.clone(),
            ctx,
            scrolled,
            jump_revealer,
            jump_icon,
            following: Cell::new(true),
            centring: RefCell::new(None),
        });
        {
            let weak = Rc::downgrade(&panel);
            jump_btn.connect_clicked(move |_| {
                if let Some(panel) = weak.upgrade() {
                    panel.following.set(true);
                    panel.centre_current(true);
                }
            });
            let adj = panel.scrolled.vadjustment();
            let weak = Rc::downgrade(&panel);
            adj.connect_value_changed(move |_| {
                if let Some(panel) = weak.upgrade() {
                    panel.update_jump();
                }
            });
            let weak = Rc::downgrade(&panel);
            adj.connect_changed(move |_| {
                if let Some(panel) = weak.upgrade() {
                    panel.update_jump();
                }
            });
        }
        {
            let weak = Rc::downgrade(&panel);
            add_btn.connect_clicked(move |btn| {
                if let Some(panel) = weak.upgrade() {
                    let p = panel.clone();
                    AddToPlaylistPopover::show(&panel.ctx, btn, move |pid| p.add_all_to_playlist(&pid));
                }
            });
        }

        {
            let weak = Rc::downgrade(&panel);
            factory.connect_setup(move |_, item| {
                let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
                    return;
                };
                if let Some(panel) = weak.upgrade() {
                    item.set_child(Some(&panel.build_row(item)));
                }
            });
        }

        {
            let player = player.clone();
            shuffle_btn.connect_clicked(move |_| player.toggle_shuffle());
        }
        {
            let player = player.clone();
            repeat_btn.connect_clicked(move |_| {
                let next = match player.repeat_mode() {
                    RepeatMode::Off => RepeatMode::All,
                    RepeatMode::All => RepeatMode::Track,
                    RepeatMode::Track => RepeatMode::Off,
                };
                player.set_repeat(next);
            });
        }
        {
            let player = player.clone();
            clear_btn.connect_clicked(move |_| player.clear_queue());
        }

        // Keep the playing row in the middle when it changes or the panel appears,
        // unless the listener has scrolled away from it to look at something else.
        {
            let weak = Rc::downgrade(&panel);
            state.connect_notify_local(Some("current-index"), move |_, _| {
                if let Some(panel) = weak.upgrade() {
                    if panel.following.get() {
                        panel.scroll_to_current_later(true);
                    } else {
                        panel.update_jump();
                    }
                }
            });
        }
        {
            let weak = Rc::downgrade(&panel);
            panel.root.connect_map(move |_| {
                if let Some(panel) = weak.upgrade() {
                    if panel.list.model().is_none() {
                        panel.list.set_model(Some(&panel.selection));
                    }
                    panel.following.set(true);
                    panel.scroll_to_current_later(false);
                }
            });
            let weak = Rc::downgrade(&panel);
            panel.root.connect_unmap(move |_| {
                let weak = weak.clone();
                glib::idle_add_local_once(move || {
                    if let Some(panel) = weak.upgrade().filter(|p| !p.root.is_mapped()) {
                        panel.list.set_model(None::<&gtk::SelectionModel>);
                    }
                });
            });
        }

        panel
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    fn scroll_to_current_later(self: &Rc<Self>, animate: bool) {
        let weak = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            if let Some(panel) = weak.upgrade().filter(|p| p.root.is_mapped()) {
                panel.centre_current(animate);
            }
        });
    }

    /// Where the playing row sits in the list, as (top, height) in the adjustment's
    /// pixels. Every row is the same height, so the list's length divides evenly.
    fn current_row(&self) -> Option<(f64, f64)> {
        let state = self.player.state();
        let (index, length) = (state.current_index(), state.queue_length());
        let adj = self.scrolled.vadjustment();
        if index < 0 || index as u32 >= length || adj.upper() <= 0.0 {
            return None;
        }
        let height = adj.upper() / f64::from(length);
        Some((f64::from(index) * height, height))
    }

    /// Put the playing row in the middle of the list, or as near as the ends allow:
    /// the first rows stay at the top until the queue has moved half a screen down.
    fn centre_current(self: &Rc<Self>, animate: bool) {
        if let Some(animation) = self.centring.take() {
            animation.pause();
        }
        let adj = self.scrolled.vadjustment();
        let Some((top, height)) = self.current_row().filter(|_| adj.upper() > adj.page_size()) else {
            // Not laid out yet, or everything fits. No FOCUS flag: on phones the panel sits in the
            // drawer's viewport, which follows focus and shifted the list, leaving blank rows.
            let index = self.player.state().current_index();
            if index >= 0 && (index as u32) < self.player.state().queue_length() {
                self.list.scroll_to(index as u32, gtk::ListScrollFlags::NONE, None);
            }
            self.update_jump();
            return;
        };
        let target = (top - (adj.page_size() - height) / 2.0).clamp(adj.lower(), adj.upper() - adj.page_size());
        // A long way off, an eased scroll would build every row it passes.
        if !animate || (adj.value() - target).abs() > adj.page_size() * 3.0 {
            adj.set_value(target);
            self.update_jump();
            return;
        }
        let moving = adj.clone();
        let animation = adw::TimedAnimation::builder()
            .widget(&self.scrolled)
            .value_from(adj.value())
            .value_to(target)
            .duration(250)
            .easing(adw::Easing::EaseOutCubic)
            .target(&adw::CallbackAnimationTarget::new(move |value| moving.set_value(value)))
            .build();
        let weak = Rc::downgrade(self);
        animation.connect_done(move |_| {
            if let Some(panel) = weak.upgrade() {
                panel.centring.take();
                panel.update_jump();
            }
        });
        animation.play();
        self.centring.replace(Some(animation));
    }

    /// Show the arrow button while the playing row is out of sight, pointing the way to it.
    fn update_jump(&self) {
        // The scroll back to the row passes through "out of sight" on its way.
        if self.centring.borrow().is_some() {
            return;
        }
        let adj = self.scrolled.vadjustment();
        let away = self.current_row().and_then(|(top, height)| {
            if top + height <= adj.value() {
                Some("go-up-symbolic")
            } else if top >= adj.value() + adj.page_size() {
                Some("go-down-symbolic")
            } else {
                None
            }
        });
        self.following.set(away.is_none());
        if let Some(icon) = away {
            self.jump_icon.set_icon_name(Some(icon));
        }
        self.jump_revealer.set_reveal_child(away.is_some());
        self.jump_revealer.set_can_target(away.is_some());
    }

    /// Row layout with expression bindings rooted at the list item, plus gestures and DnD.
    fn build_row(self: &Rc<Self>, item: &gtk::ListItem) -> gtk::Box {
        let row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .css_classes(["queue-row", "flat"])
            .build();
        let entry = item.property_expression("item");

        let handle = gtk::Image::builder()
            .icon_name("list-drag-handle-symbolic")
            .css_classes(["dim-label", "drag-handle"])
            // 8px to the sidebar's edge with the cell's 2px padding, the same as to the cover.
            .margin_start(6)
            .build();
        row.append(&handle);

        // The cover, with a badge over it: the position while the pointer is on
        // the row, the play state on the current one. See `.queue-badge`.
        let cover = CoverImage::new(self.ctx.net.clone(), COVER_SIZE);
        let indicator = gtk::Stack::new();
        let index_label = gtk::Label::builder().css_classes(["caption-heading", "numeric"]).build();
        let playing_icon = gtk::Image::builder()
            .icon_name("media-playback-pause-symbolic")
            .build();
        indicator.add_named(&index_label, Some("index"));
        indicator.add_named(&playing_icon, Some("playing"));
        // The cover goes dark under the badge through CSS, see `.queue-cover`.
        let art = gtk::Overlay::builder()
            .valign(gtk::Align::Center)
            .margin_end(4)
            .overflow(gtk::Overflow::Hidden)
            .css_classes(["queue-cover"])
            .child(cover.widget())
            .build();
        let badge = adw::Bin::builder().css_classes(["queue-badge"]).child(&indicator).build();
        art.add_overlay(&badge);
        row.append(&art);

        let info = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .hexpand(true)
            .build();
        let title = gtk::Label::builder()
            .halign(gtk::Align::Start)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["body"])
            .build();
        let artist = gtk::Label::builder()
            .halign(gtk::Align::Start)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["caption", "dim-label"])
            .build();
        info.append(&title);
        info.append(&artist);
        row.append(&info);

        entry
            .chain_property::<QueueEntry>("title")
            .bind(&title, "label", gtk::Widget::NONE);
        entry
            .chain_property::<QueueEntry>("artist")
            .chain_closure::<String>(glib::closure!(|_: Option<glib::Object>, artist: String| {
                if artist.is_empty() {
                    // Translators: shown in place of the artist of a queued song that has none.
                    tr!("Unknown")
                } else {
                    artist
                }
            }))
            .bind(&artist, "label", gtk::Widget::NONE);
        entry
            .chain_property::<QueueEntry>("index")
            .chain_closure::<String>(glib::closure!(|_: Option<glib::Object>, index: u32| (index
                + 1)
            .to_string()))
            .bind(&index_label, "label", gtk::Widget::NONE);
        entry
            .chain_property::<QueueEntry>("playing")
            .chain_closure::<String>(glib::closure!(|_: Option<glib::Object>, playing: bool| {
                if playing { "playing" } else { "index" }
            }))
            .bind(&indicator, "visible-child-name", gtk::Widget::NONE);
        entry
            .chain_property::<QueueEntry>("paused")
            .chain_closure::<String>(glib::closure!(|_: Option<glib::Object>, paused: bool| {
                if paused {
                    "media-playback-start-symbolic"
                } else {
                    "media-playback-pause-symbolic"
                }
            }))
            .bind(&playing_icon, "icon-name", gtk::Widget::NONE);
        entry
            .chain_property::<QueueEntry>("playing")
            .chain_closure::<glib::StrV>(glib::closure!(
                |_: Option<glib::Object>, playing: bool| {
                    if playing {
                        glib::StrV::from(["queue-row", "playing"])
                    } else {
                        glib::StrV::from(["queue-row", "flat"])
                    }
                }
            ))
            .bind(&row, "css-classes", gtk::Widget::NONE);

        let weak_item = item.downgrade();
        let entry_of = move || {
            weak_item
                .upgrade()
                .and_then(|i| i.item())
                .and_downcast::<QueueEntry>()
        };

        // Rows are recycled: the cover follows whichever entry the row shows now,
        // and that entry's address when a fuller copy of the track arrives.
        {
            let entry_of = entry_of.clone();
            let watch = entry.chain_property::<QueueEntry>("thumbnail-url").watch(gtk::Widget::NONE, move || {
                match entry_of().filter(|e| !e.thumbnail_url().is_empty()) {
                    Some(e) => cover.load_track(&e.video_id(), &e.thumbnail_url()),
                    None => cover.clear(),
                }
            });
            row.connect_destroy(move |_| watch.unwatch());
        }

        // Left click plays the row unless it is already current.
        {
            let entry_of = entry_of.clone();
            let player = self.player.clone();
            let click = gtk::GestureClick::builder()
                .button(gdk::BUTTON_PRIMARY)
                .build();
            click.connect_released(move |_, _, _, _| {
                if let Some(entry) = entry_of() {
                    if entry.index() as i32 != player.state().current_index() {
                        player.play_queue_index(entry.index() as usize);
                    }
                }
            });
            row.add_controller(click);
        }

        // Right click or long press opens the song menu with Remove from Queue.
        {
            let open_menu = {
                let entry_of = entry_of.clone();
                let player = self.player.clone();
                let ctx = self.ctx.clone();
                let row = row.clone();
                Rc::new(move |x: f64, y: f64| {
                    let Some(entry) = entry_of() else { return };
                    let index = entry.index() as usize;
                    let remove = {
                        let player = player.clone();
                        MenuAction::new(&tr!("Remove from Queue"), Section::Remove, move || {
                            player.remove_from_queue(index)
                        })
                    };
                    let opts = SongMenuOptions {
                        prefix: "q",
                        hide: &["play_next", "add_to_queue"],
                        extras: vec![remove],
                        nav: Some(ctx.nav.clone()),
                        ctx: Some(ctx.clone()),
                        ..Default::default()
                    };
                    show_song_menu(&row, x, y, &entry.track(), &player, opts);
                })
            };
            let right = gtk::GestureClick::builder()
                .button(gdk::BUTTON_SECONDARY)
                .build();
            let open = open_menu.clone();
            right.connect_released(move |_, _, x, y| open(x, y));
            row.add_controller(right);
            let long = gtk::GestureLongPress::new();
            let open = open_menu.clone();
            long.connect_pressed(move |_, x, y| open(x, y));
            row.add_controller(long);
        }

        // Drag the handle, drop on another row to reorder.
        {
            let source = gtk::DragSource::builder()
                .actions(gdk::DragAction::MOVE)
                .build();
            let entry_for_prepare = entry_of.clone();
            source.connect_prepare(move |_, _, _| {
                entry_for_prepare()
                    .map(|e| gdk::ContentProvider::for_value(&e.index().to_string().to_value()))
            });
            let row_for_icon = row.clone();
            source.connect_drag_begin(move |source, _| {
                let paintable = gtk::WidgetPaintable::new(Some(&row_for_icon));
                source.set_icon(Some(&paintable), 0, 0);
            });
            handle.add_controller(source);

            let target = gtk::DropTarget::new(glib::Type::STRING, gdk::DragAction::MOVE);
            let entry_for_drop = entry_of.clone();
            let player = self.player.clone();
            target.connect_drop(move |_, value, _, _| {
                let Ok(text) = value.get::<String>() else {
                    return false;
                };
                let Ok(from) = text.parse::<usize>() else {
                    return false;
                };
                let Some(entry) = entry_for_drop() else {
                    return false;
                };
                let to = entry.index() as usize;
                if from != to {
                    player.move_queue_item(from, to);
                }
                true
            });
            row.add_controller(target);
        }

        row
    }
}

impl QueuePanel {
    /// Port of _do_add_all_to_playlist: every queued track into the chosen playlist.
    fn add_all_to_playlist(self: &Rc<Self>, playlist_id: &str) {
        crate::ui::playlist_ops::add_tracks(&self.ctx, self.root.upcast_ref(), playlist_id.to_owned(), self.player.queue_tracks());
    }
}

/// The header caption: "42 tracks · 2 h 31 min". Tracks of unknown length add nothing to the time.
fn queue_summary(tracks: u32, seconds: u32) -> String {
    let minutes = seconds / 60;
    match (minutes / 60, minutes % 60) {
        (0, 0) => trn!("{n} track", "{n} tracks", tracks),
        // Translators: the queue's track count and its length, {m} is minutes.
        (0, m) => trn!("{n} track · {m} min", "{n} tracks · {m} min", tracks, m),
        // Translators: the queue's track count and its length, {h} is hours and {m} is minutes.
        (h, m) => trn!("{n} track · {h} h {m} min", "{n} tracks · {h} h {m} min", tracks, h, m),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_caption_drops_the_time_when_no_track_has_a_length() {
        assert_eq!(queue_summary(0, 0), "0 tracks");
        assert_eq!(queue_summary(1, 0), "1 track");
    }

    #[test]
    fn the_caption_counts_hours_once_the_queue_passes_one() {
        assert_eq!(queue_summary(12, 59 * 60 + 59), "12 tracks · 59 min");
        assert_eq!(queue_summary(42, 2 * 3600 + 31 * 60), "42 tracks · 2 h 31 min");
    }
}
