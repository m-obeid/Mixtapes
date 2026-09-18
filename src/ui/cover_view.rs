//! Port of ui/desktop_cover_view.py: the full-window desktop player with a
//! Player / Lyrics toggle, a big cover, transport over the visualizer, and a
//! lyrics sidebar in an overlay split view.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::gdk;

use crate::model::{LikeStatus, PlaybackStatus, VideoId};
use crate::ui::context::{NavRequest, UiContext};
use crate::ui::like_button::LikeButton;
use crate::ui::marquee::MarqueeLabel;
use crate::ui::widgets::cover_picture::CoverPicture;
use crate::ui::widgets::lyrics_view::LyricsView;
use crate::ui::widgets::transport::Transport;
use crate::ui::widgets::visualizer::Visualizer;

pub struct DesktopCoverView {
    root: adw::Bin,
    toggle_nav: adw::ToggleGroup,
    split: adw::OverlaySplitView,
    cover_clamp: adw::Clamp,
    cover: Rc<CoverPicture>,
    title: Rc<MarqueeLabel>,
    artists_box: gtk::Box,
    like: Rc<LikeButton>,
    #[allow(dead_code)]
    transport: Rc<Transport>,
    visualizer: Rc<Visualizer>,
    /// A metadata update is already queued for the next idle.
    metadata_pending: Cell<bool>,
    /// Kept alive here. The widget tree only holds its root box.
    #[allow(dead_code)]
    lyrics_view: Rc<LyricsView>,
    more_btn: gtk::MenuButton,
    ctx: Rc<UiContext>,
    lyrics_intent: Cell<bool>,
    suppress_sync: Cell<bool>,
    on_dismiss: RefCell<Option<Rc<dyn Fn()>>>,
    on_queue_click: RefCell<Option<Rc<dyn Fn()>>>,
}

impl DesktopCoverView {
    pub fn new(ctx: Rc<UiContext>) -> Rc<Self> {
        let player = ctx.player.clone();
        let root = adw::Bin::new();
        let toolbar = adw::ToolbarView::builder()
            .hexpand(true)
            .vexpand(true)
            .build();
        root.set_child(Some(&toolbar));

        let toggle_nav = adw::ToggleGroup::builder()
            .css_classes(["round"])
            .halign(gtk::Align::Center)
            .margin_top(8)
            .margin_bottom(8)
            .build();
        toggle_nav.add(
            adw::Toggle::builder()
                .name("player")
                .label("Player")
                .icon_name("folder-music-symbolic")
                .build(),
        );
        toggle_nav.add(
            adw::Toggle::builder()
                .name("lyrics")
                .label("Lyrics")
                .icon_name("format-justify-fill-symbolic")
                .build(),
        );
        toolbar.add_top_bar(&toggle_nav);

        // -- cover with the hover-revealed lyrics toggle -----------------
        let cover = CoverPicture::new(ctx.net.clone());
        cover.widget().add_css_class("cover-desktop");
        cover.widget().set_hexpand(true);
        cover.widget().set_vexpand(true);

        let cover_overlay = gtk::Overlay::builder().child(cover.widget()).build();
        let cover_frame = gtk::AspectFrame::builder()
            .ratio(1.0)
            .obey_child(false)
            .margin_bottom(16)
            .vexpand(true)
            .hexpand(true)
            .overflow(gtk::Overflow::Hidden)
            .child(&cover_overlay)
            .build();

        // -- metadata ----------------------------------------------------
        let meta_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .hexpand(true)
            .valign(gtk::Align::Center)
            .margin_bottom(8)
            .build();
        let text_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .hexpand(true)
            .valign(gtk::Align::Center)
            .build();
        let title = MarqueeLabel::new();
        title.set_label("Not Playing");
        title.add_css_class("title-3");
        let artists_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(2)
            .halign(gtk::Align::Start)
            .build();
        text_box.append(title.widget());
        text_box.append(&artists_box);
        let like = LikeButton::new(player.clone());
        like.widget().set_visible(false);
        meta_row.append(&text_box);
        meta_row.append(like.widget());

        // -- transport over the visualizer -------------------------------
        let visualizer = Visualizer::new(&ctx, 80);
        visualizer.widget().set_hexpand(true);
        visualizer.widget().set_vexpand(true);
        visualizer.widget().set_can_target(false);
        visualizer.widget().add_css_class("cover-visualizer");

        let transport = Transport::new(player.clone(), 48, 20, 0);
        let more_btn = gtk::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .css_classes(["flat", "circular"])
            .valign(gtk::Align::Center)
            .build();
        let buttons_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(16)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .build();
        buttons_row.append(&transport.vol_btn);
        buttons_row.append(&transport.prev_btn);
        buttons_row.append(&transport.play_btn);
        buttons_row.append(&transport.next_btn);
        buttons_row.append(&more_btn);

        let time_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .hexpand(true)
            .build();
        time_row.append(&transport.pos_label);
        time_row.append(&gtk::Box::builder().hexpand(true).build());
        time_row.append(&transport.dur_label);
        let progress_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .hexpand(true)
            .build();
        progress_row.append(&transport.scale);

        let controls_content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .hexpand(true)
            .valign(gtk::Align::Center)
            .build();
        controls_content.append(&progress_row);
        controls_content.append(&time_row);
        controls_content.append(&buttons_row);
        let overlay_base = adw::Bin::builder().hexpand(true).height_request(85).build();
        let controls_overlay = gtk::Overlay::builder()
            .hexpand(true)
            .valign(gtk::Align::Center)
            .child(&overlay_base)
            .build();
        controls_overlay.add_overlay(visualizer.widget());
        controls_overlay.add_overlay(&controls_content);

        let player_controls = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .margin_top(16)
            .hexpand(true)
            .valign(gtk::Align::Center)
            .build();
        player_controls.append(&cover_frame);
        player_controls.append(&meta_row);
        player_controls.append(&controls_overlay);
        let cover_column = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .hexpand(true)
            .vexpand(true)
            .build();
        cover_column.append(&player_controls);
        let cover_clamp = adw::Clamp::builder()
            .maximum_size(512)
            .hexpand(true)
            .vexpand(true)
            .child(&cover_column)
            .build();
        let cover_outer = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .hexpand(true)
            .vexpand(true)
            .margin_bottom(32)
            .margin_start(48)
            .margin_end(48)
            .build();
        cover_outer.append(&cover_clamp);

        // -- lyrics sidebar ----------------------------------------------
        let lyrics_outer = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .hexpand(true)
            .vexpand(true)
            .margin_top(16)
            .margin_bottom(32)
            .margin_end(24)
            .build();
        let lyrics_view = LyricsView::new(ctx.clone());
        lyrics_outer.append(lyrics_view.widget());
        let split = adw::OverlaySplitView::builder()
            .css_classes(["lyrics-split"])
            .content(&cover_outer)
            .sidebar(&lyrics_outer)
            .sidebar_position(gtk::PackType::End)
            .show_sidebar(false)
            .collapsed(false)
            .sidebar_width_fraction(0.55)
            .min_sidebar_width(360.0)
            .max_sidebar_width(900.0)
            .build();

        let collapse_btn = gtk::Button::builder()
            .icon_name("go-down-symbolic")
            .css_classes(["flat", "circular"])
            .valign(gtk::Align::End)
            .halign(gtk::Align::End)
            .tooltip_text("Collapse player")
            .margin_end(24)
            .margin_bottom(24)
            .build();
        let queue_btn = gtk::Button::builder()
            .icon_name("music-queue-symbolic")
            .css_classes(["flat", "circular"])
            .valign(gtk::Align::End)
            .halign(gtk::Align::End)
            .tooltip_text("Queue")
            .margin_end(66)
            .margin_bottom(24)
            .build();
        let view_overlay = gtk::Overlay::builder()
            .hexpand(true)
            .vexpand(true)
            .child(&split)
            .build();
        view_overlay.add_overlay(&collapse_btn);
        view_overlay.add_overlay(&queue_btn);

        let bp_bin = adw::BreakpointBin::builder()
            .width_request(150)
            .height_request(150)
            .child(&view_overlay)
            .build();
        let collapse_bp = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
            adw::BreakpointConditionLengthType::MaxWidth,
            735.0,
            adw::LengthUnit::Px,
        ));
        collapse_bp.add_setter(&split, "collapsed", Some(&true.to_value()));
        bp_bin.add_breakpoint(collapse_bp);
        toolbar.set_content(Some(&bp_bin));

        let this = Rc::new(Self {
            root,
            toggle_nav,
            split,
            cover_clamp,
            cover,
            title,
            artists_box,
            like,
            transport,
            visualizer,
            metadata_pending: Cell::new(false),
            lyrics_view,
            more_btn: more_btn.clone(),
            ctx,
            lyrics_intent: Cell::new(false),
            suppress_sync: Cell::new(false),
            on_dismiss: RefCell::new(None),
            on_queue_click: RefCell::new(None),
        });

        {
            let weak = Rc::downgrade(&this);
            collapse_btn.connect_clicked(move |_| {
                if let Some(v) = weak.upgrade() {
                    v.dismiss();
                }
            });
            let weak = Rc::downgrade(&this);
            queue_btn.connect_clicked(move |_| {
                if let Some(v) = weak.upgrade() {
                    if let Some(f) = v.on_queue_click.borrow().as_ref() {
                        f();
                    }
                }
            });
        }
        this.connect_lyrics(&cover_overlay);
        this.bind_state();
        this.refresh_metadata();
        this.refresh_more_menu();

        let initial = this
            .ctx
            .paths
            .read_prefs()
            .get("lyrics_shown_desktop")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if initial {
            this.split.set_show_sidebar(true);
        }
        this.lyrics_intent.set(initial);
        this.sync_nav_toggles(initial);
        this
    }

    pub fn widget(&self) -> &adw::Bin {
        &self.root
    }

    pub fn set_on_dismiss(&self, f: impl Fn() + 'static) {
        self.on_dismiss.replace(Some(Rc::new(f)));
    }

    pub fn set_on_queue_click(&self, f: impl Fn() + 'static) {
        self.on_queue_click.replace(Some(Rc::new(f)));
    }

    fn dismiss(&self) {
        if let Some(f) = self.on_dismiss.borrow().as_ref() {
            f();
        }
    }

    // -- lyrics split -----------------------------------------------------

    fn connect_lyrics(self: &Rc<Self>, _cover_overlay: &gtk::Overlay) {
        let weak = Rc::downgrade(self);
        self.toggle_nav.connect_active_name_notify(move |group| {
            if let Some(v) = weak.upgrade() {
                if !v.suppress_sync.get() {
                    v.apply_lyrics_state(group.active_name().as_deref() == Some("lyrics"));
                }
            }
        });

        let weak = Rc::downgrade(self);
        self.split.connect_show_sidebar_notify(move |split| {
            let Some(v) = weak.upgrade() else { return };
            if v.suppress_sync.get() {
                return;
            }
            let actual = split.shows_sidebar();
            if split.is_collapsed() && v.lyrics_intent.get() && !actual {
                v.suppress_sync.set(true);
                split.set_show_sidebar(true);
                v.suppress_sync.set(false);
                return;
            }
            v.lyrics_intent.set(actual);
            v.sync_nav_toggles(actual);
            v.cover_clamp
                .set_opacity(if split.is_collapsed() && actual {
                    0.0
                } else {
                    1.0
                });
        });

        let weak = Rc::downgrade(self);
        self.split.connect_collapsed_notify(move |split| {
            let Some(v) = weak.upgrade() else { return };
            let collapsed = split.is_collapsed();
            if collapsed && v.lyrics_intent.get() {
                v.suppress_sync.set(true);
                split.set_show_sidebar(true);
                v.suppress_sync.set(false);
            }
            v.cover_clamp
                .set_opacity(if collapsed && v.lyrics_intent.get() {
                    0.0
                } else {
                    1.0
                });
        });

        let weak = Rc::downgrade(self);
        self.root.connect_map(move |_| {
            if let Some(v) = weak.upgrade() {
                v.visualizer
                    .set_active(v.ctx.player.state().status() == PlaybackStatus::Playing);
            }
        });
    }

    fn apply_lyrics_state(&self, show: bool) {
        self.lyrics_intent.set(show);
        self.cover_clamp
            .set_opacity(if self.split.is_collapsed() && show {
                0.0
            } else {
                1.0
            });
        self.suppress_sync.set(true);
        self.split.set_show_sidebar(show);
        self.suppress_sync.set(false);
        self.sync_nav_toggles(show);
        self.ctx.paths.update_prefs(|p| {
            p.insert("lyrics_shown_desktop".into(), serde_json::Value::Bool(show));
        });
    }

    fn sync_nav_toggles(&self, shown: bool) {
        self.suppress_sync.set(true);
        let target = if shown { "lyrics" } else { "player" };
        if self.toggle_nav.active_name().as_deref() != Some(target) {
            self.toggle_nav.set_active_name(Some(target));
        }
        self.suppress_sync.set(false);
    }

    // -- state ------------------------------------------------------------

    fn bind_state(self: &Rc<Self>) {
        let state = self.ctx.player.state();
        for prop in [
            "title",
            "artist",
            "thumbnail-url",
            "video-id",
            "like-status",
        ] {
            let weak = Rc::downgrade(self);
            // A track change moves all five properties in a row. One update on the
            // next idle covers them, where each used to rebuild the labels and the menu.
            state.connect_notify_local(Some(prop), move |_, _| {
                let Some(v) = weak.upgrade() else { return };
                if v.metadata_pending.replace(true) {
                    return;
                }
                let weak = weak.clone();
                glib::idle_add_local_once(move || {
                    if let Some(v) = weak.upgrade() {
                        v.metadata_pending.set(false);
                        v.refresh_metadata();
                        v.refresh_more_menu();
                    }
                });
            });
        }
        let weak = Rc::downgrade(self);
        state.connect_notify_local(Some("status"), move |state, _| {
            if let Some(v) = weak.upgrade() {
                v.visualizer
                    .set_active(state.status() == PlaybackStatus::Playing);
            }
        });
    }

    fn refresh_metadata(&self) {
        let state = self.ctx.player.state();
        let title = state.title();
        self.title.set_label(if title.is_empty() {
            "Not Playing"
        } else {
            &title
        });
        self.cover.load(&state.thumbnail_url());
        while let Some(child) = self.artists_box.first_child() {
            self.artists_box.remove(&child);
        }
        let track = self.ctx.player.current_track();
        let artists: Vec<(Option<String>, String)> =
            match track.as_ref().filter(|t| !t.artists.is_empty()) {
                Some(t) => t
                    .artists
                    .iter()
                    .map(|a| (a.id.clone(), a.name.clone()))
                    .collect(),
                _ => vec![(None, state.artist())],
            };
        let count = artists.len();
        for (i, (id, name)) in artists.into_iter().enumerate() {
            if name.is_empty() {
                continue;
            }
            let label = gtk::Label::builder()
                .label(&name)
                .css_classes(["heading"])
                .opacity(0.7)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .build();
            let btn = gtk::Button::builder()
                .css_classes(["flat", "link-btn"])
                .has_frame(false)
                .child(&label)
                .build();
            btn.set_cursor(gdk::Cursor::from_name("pointer", None).as_ref());
            let ctx = self.ctx.clone();
            let dismiss = self.on_dismiss.borrow().clone();
            btn.connect_clicked(move |_| {
                ctx.nav.go(NavRequest::Artist {
                    id: id.clone(),
                    name: name.clone(),
                });
                if let Some(f) = &dismiss {
                    f();
                }
            });
            self.artists_box.append(&btn);
            if i + 1 < count {
                self.artists_box.append(
                    &gtk::Label::builder()
                        .label(", ")
                        .css_classes(["heading"])
                        .opacity(0.7)
                        .build(),
                );
            }
        }
        let video_id = state.video_id();
        if video_id.is_empty() {
            self.like.set_data(None, None);
        } else {
            self.like.set_data(
                Some(VideoId(video_id)),
                Some(LikeStatus::parse(&state.like_status())),
            );
        }
    }

    /// The menu is rebuilt whenever the track changes, like the Python view.
    /// A menu button with no model is insensitive, so a menu built on activate
    /// could never be opened.
    fn refresh_more_menu(self: &Rc<Self>) {
        let Some(track) = self.ctx.player.current_track() else {
            self.more_btn.set_menu_model(gtk::gio::MenuModel::NONE);
            return;
        };
        let this = self.clone();
        let extras = vec![crate::ui::context_menu::MenuAction::new("Stream Info (Debug)", crate::ui::context_menu::Section::Debug, move || this.show_stream_info())];
        let opts = crate::ui::context_menu::SongMenuOptions {
            prefix: "cv",
            hide: ["play_next", "add_to_queue", "goto_artist", "goto_album"].as_slice(),
            nav: Some(self.ctx.nav.clone()),
            ctx: Some(self.ctx.clone()),
            extras,
            ..Default::default()
        };
        let model = crate::ui::context_menu::build_song_menu(&self.more_btn, &track, &self.ctx.player, opts);
        self.more_btn.set_menu_model(model.as_ref());
    }

    /// Port of _show_stream_info: what is playing and how the pipeline sees it.
    pub fn visualizer(&self) -> &Rc<Visualizer> {
        &self.visualizer
    }

    pub fn show_stream_info(self: &Rc<Self>) {
        crate::ui::expanded_player::present_stream_info(&self.ctx, self.root.upcast_ref::<gtk::Widget>());
    }
}
