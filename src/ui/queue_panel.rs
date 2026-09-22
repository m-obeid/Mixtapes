//! Queue sidebar. Port of ui/queue_panel.py: toolbar header with count,
//! shuffle and repeat toggles, drag-and-drop reordering, right-click menu,
//! and a pause-aware playing indicator. Rows follow `QueueEntry` properties
//! through expression bindings, so recycling needs no unbind code.

use std::rc::Rc;

use gtk::{gdk, gio, glib, prelude::*};

use crate::model::RepeatMode;
use crate::net::library;
use crate::net::playlists::editable_playlists;
use crate::net::ytmusic::AuthState;
use crate::player::Player;
use crate::state::QueueEntry;
use crate::ui::context::UiContext;
use crate::ui::context_menu::{MenuAction, Section, SongMenuOptions, show_song_menu};
use crate::ui::widgets::add_to_playlist::AddToPlaylistPopover;

pub struct QueuePanel {
    root: gtk::Box,
    pub header_bar: adw::HeaderBar,
    list: gtk::ListView,
    player: Rc<Player>,
    more_btn: gtk::MenuButton,
    more_menu: gio::Menu,
    ctx: Rc<UiContext>,
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
        let title_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .valign(gtk::Align::Center)
            .build();
        let title = gtk::Label::builder()
            .label("Queue")
            .css_classes(["sidebar-title"])
            .halign(gtk::Align::Center)
            .build();
        let count = gtk::Label::builder()
            .css_classes(["caption-2", "dim-label"])
            .halign(gtk::Align::Center)
            .opacity(0.6)
            .build();
        state
            .bind_property("queue-length", &count, "label")
            .transform_to(|_, n: u32| Some(format!("{n} tracks")))
            .sync_create()
            .build();
        title_box.append(&title);
        title_box.append(&count);
        header_bar.set_title_widget(Some(&title_box));

        let shuffle_btn = gtk::ToggleButton::builder()
            .icon_name("media-playlist-shuffle-symbolic")
            .tooltip_text("Shuffle Queue")
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
        header_bar.pack_start(&shuffle_btn);

        let repeat_btn = gtk::Button::builder()
            .icon_name("media-playlist-consecutive-symbolic")
            .tooltip_text("Repeat Mode")
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
        header_bar.pack_start(&repeat_btn);

        let clear_btn = gtk::Button::builder().label("Clear").build();
        header_bar.pack_end(&clear_btn);

        let more_btn = gtk::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .tooltip_text("More Options")
            .build();
        let more_menu = gio::Menu::new();
        more_btn.set_menu_model(Some(&more_menu));
        header_bar.pack_end(&more_btn);

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header_bar);
        root.append(&toolbar);

        // -- list --------------------------------------------------------
        let factory = gtk::SignalListItemFactory::new();
        let selection = gtk::NoSelection::new(Some(state.queue_model()));
        let list = gtk::ListView::builder()
            .model(&selection)
            .factory(&factory)
            .build();
        let scrolled = gtk::ScrolledWindow::builder()
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&list)
            .build();
        crate::ui::suppress_hover_while_scrolling(&scrolled);
        root.append(&scrolled);

        let panel = Rc::new(Self {
            root,
            header_bar,
            list,
            player: player.clone(),
            more_btn,
            more_menu,
            ctx,
        });
        {
            // "queue" actions, as insert_action_group("queue", ...) did.
            let group = gio::SimpleActionGroup::new();
            let action = gio::SimpleAction::new("show_add_all_to_playlist", None);
            let weak = Rc::downgrade(&panel);
            action.connect_activate(move |_, _| {
                if let Some(panel) = weak.upgrade() {
                    let p = panel.clone();
                    AddToPlaylistPopover::show(&panel.ctx, &panel.more_btn, move |pid| {
                        p.add_all_to_playlist(&pid)
                    });
                }
            });
            group.add_action(&action);
            panel.root.insert_action_group("queue", Some(&group));
            let weak = Rc::downgrade(&panel);
            panel.root.connect_map(move |_| {
                if let Some(panel) = weak.upgrade() {
                    panel.refresh_playlists_menu();
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

        // Keep the playing row in view when it changes or the panel appears.
        {
            let weak = Rc::downgrade(&panel);
            state.connect_notify_local(Some("current-index"), move |_, _| {
                if let Some(panel) = weak.upgrade() {
                    panel.scroll_to_current_later();
                }
            });
        }
        {
            let weak = Rc::downgrade(&panel);
            panel.root.connect_map(move |_| {
                if let Some(panel) = weak.upgrade() {
                    panel.scroll_to_current_later();
                }
            });
        }

        panel
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    fn scroll_to_current_later(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            let Some(panel) = weak.upgrade() else { return };
            if !panel.root.is_mapped() {
                return;
            }
            let index = panel.player.state().current_index();
            if index >= 0 && (index as u32) < panel.player.state().queue_length() {
                // No FOCUS flag: on phones the panel sits in the drawer's viewport, which
                // follows focus and shifted the list under the list view, leaving blank rows.
                panel
                    .list
                    .scroll_to(index as u32, gtk::ListScrollFlags::NONE, None);
            }
        });
    }

    /// Row layout with expression bindings rooted at the list item, plus gestures and DnD.
    fn build_row(self: &Rc<Self>, item: &gtk::ListItem) -> gtk::Box {
        let row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .css_classes(["queue-row", "flat"])
            .build();
        let entry = item.property_expression("item");

        let handle = gtk::Image::builder()
            .icon_name("list-drag-handle-symbolic")
            .css_classes(["dim-label", "drag-handle"])
            .margin_start(6)
            .margin_end(4)
            .build();
        row.append(&handle);

        let indicator = gtk::Stack::new();
        let index_label = gtk::Label::builder()
            .css_classes(["dim-label"])
            .width_chars(3)
            .build();
        let playing_icon = gtk::Image::builder()
            .icon_name("media-playback-pause-symbolic")
            .css_classes(["accent"])
            .build();
        indicator.add_named(&index_label, Some("index"));
        indicator.add_named(&playing_icon, Some("playing"));
        row.append(&indicator);

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
                    "Unknown".to_owned()
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
                        MenuAction::new("Remove from Queue", Section::Remove, move || {
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
    /// Port of _refresh_playlists_menu: the menu offers Add all to Playlist…
    /// only online and only when there is an editable playlist to add to.
    fn refresh_playlists_menu(self: &Rc<Self>) {
        self.more_menu.remove_all();
        if !self.ctx.local.playlist_items().is_empty() {
            self.more_menu.append(Some("Add all to Playlist…"), Some("queue.show_add_all_to_playlist"));
            return;
        }
        if !self.ctx.online.is_online() {
            return;
        }
        let account = match self.ctx.net.client().auth_state() {
            AuthState::Authenticated(info) => Some(info.name),
            _ => None,
        };
        let cached = self.ctx.net.caches().library_playlists();
        if !cached.is_empty() {
            if !editable_playlists(&cached, account.as_deref()).is_empty() {
                self.more_menu.append(
                    Some("Add all to Playlist…"),
                    Some("queue.show_add_all_to_playlist"),
                );
            }
            return;
        }
        if !self.ctx.net.client().is_authenticated() {
            return;
        }
        let handle = self
            .ctx
            .net
            .spawn(library::library_playlists(self.ctx.net.client().api()));
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Ok(Ok(playlists)) = handle.await else {
                return;
            };
            let Some(panel) = weak.upgrade() else { return };
            panel
                .ctx
                .net
                .caches()
                .set_library_playlists(playlists.clone());
            if !editable_playlists(&playlists, account.as_deref()).is_empty()
                && panel.more_menu.n_items() == 0
            {
                panel.more_menu.append(
                    Some("Add all to Playlist…"),
                    Some("queue.show_add_all_to_playlist"),
                );
            }
        });
    }

    /// Port of _do_add_all_to_playlist: every queued track into the chosen playlist.
    fn add_all_to_playlist(self: &Rc<Self>, playlist_id: &str) {
        crate::ui::playlist_ops::add_tracks(&self.ctx, self.root.upcast_ref(), playlist_id.to_owned(), self.player.queue_tracks());
    }
}
