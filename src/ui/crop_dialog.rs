//! Port of ui/crop_dialog.py: pick a square out of a picture for a playlist
//! cover. Drag the square to move it, drag its bottom-right handle to resize
//! it, and the result comes back as a 512 px square.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::gdk_pixbuf::{InterpType, Pixbuf};
use gtk::gdk;

const MAX_DISPLAY: i32 = 480;
const OUTPUT_SIZE: i32 = 512;

struct CropState {
    original: Pixbuf,
    display: Pixbuf,
    display_scale: f64,
    crop_size: f64,
    offset_x: f64,
    offset_y: f64,
    resizing: bool,
    orig_crop_size: f64,
    orig_offset_x: f64,
    orig_offset_y: f64,
}

/// Open the crop window over `parent`. `on_result` receives the cropped 512 px square.
pub fn show(parent: &impl IsA<gtk::Window>, pixbuf: Pixbuf, on_result: impl Fn(Pixbuf) + 'static) {
    let (w, h) = (pixbuf.width(), pixbuf.height());
    let display_scale = if w > MAX_DISPLAY || h > MAX_DISPLAY { MAX_DISPLAY as f64 / w.max(h) as f64 } else { 1.0 };
    let display = pixbuf.scale_simple(((w as f64) * display_scale) as i32, ((h as f64) * display_scale) as i32, InterpType::Bilinear).unwrap_or_else(|| pixbuf.clone());
    let (img_w, img_h) = (display.width() as f64, display.height() as f64);
    let crop_size = 300.0_f64.min(img_w).min(img_h);
    let state = Rc::new(RefCell::new(CropState {
        original: pixbuf,
        display,
        display_scale,
        crop_size,
        offset_x: ((img_w - crop_size) / 2.0).floor(),
        offset_y: ((img_h - crop_size) / 2.0).floor(),
        resizing: false,
        orig_crop_size: 0.0,
        orig_offset_x: 0.0,
        orig_offset_y: 0.0,
    }));

    let window = adw::Window::builder().title("Edit Playlist Cover").transient_for(parent).modal(true).default_width(540).default_height(680).build();
    let toolbar = adw::ToolbarView::new();
    window.set_content(Some(&toolbar));
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new("Crop Playlist Cover", "")));
    toolbar.add_top_bar(&header);

    let main_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(24).margin_top(24).margin_bottom(24).margin_start(24).margin_end(24).valign(gtk::Align::Center).build();
    toolbar.set_content(Some(&main_box));
    main_box.append(&gtk::Label::builder().label("Select the square area you want to use.").css_classes(["title-3"]).build());
    let frame = gtk::Frame::builder().halign(gtk::Align::Center).valign(gtk::Align::Center).build();
    main_box.append(&frame);
    let area = gtk::DrawingArea::builder().width_request(MAX_DISPLAY).height_request(MAX_DISPLAY).build();
    frame.set_child(Some(&area));

    {
        let state = state.clone();
        area.set_draw_func(move |_, cr, width, height| {
            let s = state.borrow();
            let (img_w, img_h) = (s.display.width() as f64, s.display.height() as f64);
            let draw_x = (width as f64 - img_w) / 2.0;
            let draw_y = (height as f64 - img_h) / 2.0;
            cr.save().ok();
            cr.set_source_rgb(0.9, 0.9, 0.9);
            cr.rectangle(draw_x, draw_y, img_w, img_h);
            let _ = cr.fill();
            cr.restore().ok();
            cr.set_source_pixbuf(&s.display, draw_x, draw_y);
            let _ = cr.paint();
            cr.set_source_rgba(0.0, 0.0, 0.0, 0.6);
            cr.rectangle(draw_x, draw_y, s.offset_x, img_h);
            cr.rectangle(draw_x + s.offset_x + s.crop_size, draw_y, img_w - s.offset_x - s.crop_size, img_h);
            cr.rectangle(draw_x + s.offset_x, draw_y, s.crop_size, s.offset_y);
            cr.rectangle(draw_x + s.offset_x, draw_y + s.offset_y + s.crop_size, s.crop_size, img_h - s.offset_y - s.crop_size);
            let _ = cr.fill();
            cr.set_source_rgba(1.0, 1.0, 1.0, 0.9);
            cr.set_line_width(2.0);
            cr.rectangle(draw_x + s.offset_x, draw_y + s.offset_y, s.crop_size, s.crop_size);
            let _ = cr.stroke();
            let handle_x = draw_x + s.offset_x + s.crop_size;
            let handle_y = draw_y + s.offset_y + s.crop_size;
            cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
            cr.arc(handle_x, handle_y, 8.0, 0.0, std::f64::consts::TAU);
            let _ = cr.fill();
            cr.set_source_rgba(0.0, 0.0, 0.0, 0.5);
            cr.set_line_width(1.0);
            cr.arc(handle_x, handle_y, 8.0, 0.0, std::f64::consts::TAU);
            let _ = cr.stroke();
        });
    }

    let drag = gtk::GestureDrag::new();
    {
        let state = state.clone();
        let area = area.clone();
        drag.connect_drag_begin(move |_, start_x, start_y| {
            let mut s = state.borrow_mut();
            let (img_w, img_h) = (s.display.width() as f64, s.display.height() as f64);
            let draw_x = (area.width() as f64 - img_w) / 2.0;
            let draw_y = (area.height() as f64 - img_h) / 2.0;
            let handle_x = draw_x + s.offset_x + s.crop_size;
            let handle_y = draw_y + s.offset_y + s.crop_size;
            let dist = ((start_x - handle_x).powi(2) + (start_y - handle_y).powi(2)).sqrt();
            if dist < 30.0 {
                s.resizing = true;
                s.orig_crop_size = s.crop_size;
            } else {
                s.resizing = false;
                s.orig_offset_x = s.offset_x;
                s.orig_offset_y = s.offset_y;
            }
        });
    }
    {
        let state = state.clone();
        let area = area.clone();
        drag.connect_drag_update(move |_, dx, dy| {
            let mut s = state.borrow_mut();
            let (img_w, img_h) = (s.display.width() as f64, s.display.height() as f64);
            if s.resizing {
                let mut new_size = s.orig_crop_size + dx.max(dy);
                new_size = new_size.max(50.0);
                new_size = new_size.min(img_w - s.offset_x);
                new_size = new_size.min(img_h - s.offset_y);
                s.crop_size = new_size;
            } else {
                let new_x = s.orig_offset_x + dx;
                let new_y = s.orig_offset_y + dy;
                s.offset_x = new_x.max(0.0).min(img_w - s.crop_size);
                s.offset_y = new_y.max(0.0).min(img_h - s.crop_size);
            }
            area.queue_draw();
        });
    }
    area.add_controller(drag);

    let footer = gtk::ActionBar::new();
    toolbar.add_bottom_bar(&footer);
    let cancel = gtk::Button::with_label("Cancel");
    {
        let window = window.clone();
        cancel.connect_clicked(move |_| window.close());
    }
    footer.pack_start(&cancel);
    let apply = gtk::Button::builder().label("Save & Use PNG").css_classes(["suggested-action"]).build();
    {
        let window = window.clone();
        let on_result = Rc::new(on_result);
        apply.connect_clicked(move |_| {
            let result = {
                let s = state.borrow();
                let real_size = (s.crop_size / s.display_scale) as i32;
                let real_x = ((s.offset_x / s.display_scale) as i32).clamp(0, (s.original.width() - real_size).max(0));
                let real_y = ((s.offset_y / s.display_scale) as i32).clamp(0, (s.original.height() - real_size).max(0));
                if real_size > 0 { s.original.new_subpixbuf(real_x, real_y, real_size, real_size).scale_simple(OUTPUT_SIZE, OUTPUT_SIZE, InterpType::Bilinear) } else { None }
            };
            if let Some(pixbuf) = result {
                on_result(pixbuf);
            }
            window.close();
        });
    }
    footer.pack_end(&apply);
    let _ = gdk::Display::default();
    window.present();
}
