// SPDX-License-Identifier: MIT

//! The image preview: the decoded texture fitted to the pane, zoomable and
//! pannable, asking for a sharper decode whenever the pane or zoom outgrows it.

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::Duration,
};

use gtk::{gdk, gio, glib, graphene, gsk, prelude::*, subclass::prelude::*};

use crate::services::ImagePreviewSize;

pub(super) const MARGIN: f64 = 12.0;
/// A source smaller than the pane is enlarged at most this much at fit.
const MAX_UPSCALE: f64 = 2.0;
pub(super) const MIN_ZOOM: f64 = 1.0;
pub(super) const MAX_ZOOM: f64 = 4.0;
const DOUBLE_CLICK_ZOOM: f64 = 2.0;
const WHEEL_PAN: f64 = 48.0;
/// Device pixels the placement may outgrow its texture by before a new decode.
const REFINE_GROWTH: f64 = 1.1;
const REFINE_DELAY: Duration = Duration::from_millis(150);
/// Helpers round the fitted size, so a texture this close to its box filled it.
const ROUNDING_SLACK: u32 = 2;

pub(super) type Refine = Rc<dyn Fn(ImagePreviewSize)>;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Placement {
    pub(super) x: f64,
    pub(super) y: f64,
    pub(super) width: f64,
    pub(super) height: f64,
}

/// The size at zoom 1. Only a texture that is the whole source stops enlarging
/// at `MAX_UPSCALE`; a reduced decode fills the pane until a sharper one lands.
pub(super) fn fit_size(view: (f64, f64), texture: (f64, f64), native: bool) -> (f64, f64) {
    let available = (
        (view.0 - MARGIN * 2.0).max(1.0),
        (view.1 - MARGIN * 2.0).max(1.0),
    );
    let texture = (texture.0.max(1.0), texture.1.max(1.0));
    let limit = if native { MAX_UPSCALE } else { f64::INFINITY };
    let scale = (available.0 / texture.0)
        .min(available.1 / texture.1)
        .min(limit);
    (texture.0 * scale, texture.1 * scale)
}

/// `center` is the image point, in 0..1, shown at the middle of the view.
pub(super) fn place(view: (f64, f64), fit: (f64, f64), zoom: f64, center: (f64, f64)) -> Placement {
    let (width, height) = (fit.0 * zoom, fit.1 * zoom);
    let axis = |view: f64, size: f64, center: f64| {
        if size <= view {
            (view - size) / 2.0
        } else {
            (view / 2.0 - center * size).clamp(view - size, 0.0)
        }
    };
    Placement {
        x: axis(view.0, width, center.0),
        y: axis(view.1, height, center.1),
        width,
        height,
    }
}

/// The center actually shown once `place` has kept the image against the view's edges.
pub(super) fn settled_center(
    view: (f64, f64),
    fit: (f64, f64),
    zoom: f64,
    center: (f64, f64),
) -> (f64, f64) {
    let placement = place(view, fit, zoom, center);
    (
        (view.0 / 2.0 - placement.x) / placement.width.max(1.0),
        (view.1 / 2.0 - placement.y) / placement.height.max(1.0),
    )
}

/// The center after zooming to `zoom`, keeping the image point under `anchor` still.
pub(super) fn anchored_center(
    view: (f64, f64),
    fit: (f64, f64),
    from: (f64, (f64, f64)),
    zoom: f64,
    anchor: (f64, f64),
) -> (f64, f64) {
    let (from_zoom, from_center) = from;
    let before = place(view, fit, from_zoom, from_center);
    let point = (
        (anchor.0 - before.x) / before.width.max(1.0),
        (anchor.1 - before.y) / before.height.max(1.0),
    );
    let (width, height) = ((fit.0 * zoom).max(1.0), (fit.1 * zoom).max(1.0));
    settled_center(
        view,
        fit,
        zoom,
        (
            (view.0 / 2.0 - anchor.0) / width + point.0,
            (view.1 / 2.0 - anchor.1) / height + point.1,
        ),
    )
}

/// Whether a decode for `decoded` came back smaller than its box, so it is the whole source.
pub(super) fn is_native(texture: (u32, u32), decoded: ImagePreviewSize) -> bool {
    texture.0 + ROUNDING_SLACK < decoded.width && texture.1 + ROUNDING_SLACK < decoded.height
}

/// The decode to ask for when `wanted` device pixels visibly outgrow the texture.
pub(super) fn refinement(
    texture: (u32, u32),
    decoded: ImagePreviewSize,
    wanted: (f64, f64),
) -> Option<ImagePreviewSize> {
    if is_native(texture, decoded)
        || (wanted.0 <= f64::from(texture.0) * REFINE_GROWTH
            && wanted.1 <= f64::from(texture.1) * REFINE_GROWTH)
    {
        return None;
    }
    let next = ImagePreviewSize::new(wanted.0.ceil() as i32, wanted.1.ceil() as i32);
    (next.width > decoded.width || next.height > decoded.height).then_some(next)
}

mod frame_imp {
    use super::*;

    #[derive(Default)]
    pub struct ImageFrame {
        pub(super) texture: RefCell<Option<gdk::Texture>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ImageFrame {
        const NAME: &'static str = "StrataPreviewImageFrame";
        type Type = super::ImageFrame;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for ImageFrame {
        fn constructed(&self) {
            self.parent_constructed();
            let frame = self.obj();
            frame.add_css_class("preview-image");
            frame.set_cursor_from_name(Some("grab"));
            frame.set_accessible_role(gtk::AccessibleRole::Img);
        }
    }

    impl WidgetImpl for ImageFrame {
        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let Some(texture) = self.texture.borrow().clone() else {
                return;
            };
            let frame = self.obj();
            let (width, height) = (frame.width() as f32, frame.height() as f32);
            let device_width = width * frame.scale_factor() as f32;
            let filter = if device_width < texture.width() as f32 {
                gsk::ScalingFilter::Trilinear
            } else {
                gsk::ScalingFilter::Linear
            };
            snapshot.append_scaled_texture(
                &texture,
                filter,
                &graphene::Rect::new(0.0, 0.0, width, height),
            );
        }
    }
}

glib::wrapper! {
    pub struct ImageFrame(ObjectSubclass<frame_imp::ImageFrame>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

mod imp {
    use super::*;

    pub struct ImageView {
        pub(super) frame: ImageFrame,
        pub(super) decoded: Cell<Option<ImagePreviewSize>>,
        pub(super) requested: Cell<Option<ImagePreviewSize>>,
        pub(super) zoom: Cell<f64>,
        pub(super) center: Cell<(f64, f64)>,
        pub(super) pointer: Cell<(f64, f64)>,
        pub(super) pan_start: Cell<(f64, f64)>,
        pub(super) pinch_start: Cell<f64>,
        pub(super) refine: RefCell<Option<Refine>>,
        pub(super) refine_timer: RefCell<Option<glib::SourceId>>,
        pub(super) file_drag: RefCell<Option<gtk::DragSource>>,
    }

    impl Default for ImageView {
        fn default() -> Self {
            Self {
                frame: glib::Object::new(),
                decoded: Cell::default(),
                requested: Cell::default(),
                zoom: Cell::new(MIN_ZOOM),
                center: Cell::new((0.5, 0.5)),
                pointer: Cell::default(),
                pan_start: Cell::default(),
                pinch_start: Cell::new(MIN_ZOOM),
                refine: RefCell::default(),
                refine_timer: RefCell::default(),
                file_drag: RefCell::default(),
            }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ImageView {
        const NAME: &'static str = "StrataPreviewImageView";
        type Type = super::ImageView;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for ImageView {
        fn constructed(&self) {
            self.parent_constructed();
            let view = self.obj();
            view.add_css_class("preview-image-view");
            view.set_hexpand(true);
            view.set_vexpand(true);
            view.set_overflow(gtk::Overflow::Hidden);
            self.frame.set_parent(&*view);
            view.install_controllers();
        }

        fn dispose(&self) {
            if let Some(timer) = self.refine_timer.take() {
                timer.remove();
            }
            self.frame.unparent();
        }
    }

    impl WidgetImpl for ImageView {
        fn measure(&self, _: gtk::Orientation, _: i32) -> (i32, i32, i32, i32) {
            (0, 0, -1, -1)
        }

        fn size_allocate(&self, width: i32, height: i32, _: i32) {
            let view = self.obj();
            let Some(placement) = view.placement() else {
                self.frame.allocate(0, 0, -1, None);
                return;
            };
            let scale = f64::from(view.scale_factor());
            let snap = |value: f64| (value * scale).round() / scale;
            self.frame.allocate(
                placement.width.round().max(1.0) as i32,
                placement.height.round().max(1.0) as i32,
                -1,
                Some(gsk::Transform::new().translate(&graphene::Point::new(
                    snap(placement.x) as f32,
                    snap(placement.y) as f32,
                ))),
            );
            if width > 0 && height > 0 {
                view.schedule_refine();
            }
        }
    }
}

glib::wrapper! {
    pub struct ImageView(ObjectSubclass<imp::ImageView>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl ImageView {
    pub(super) fn new() -> Self {
        glib::Object::new()
    }

    /// Where file drags start; it stands down while a zoomed image pans instead.
    pub(super) fn frame(&self) -> &ImageFrame {
        &self.imp().frame
    }

    pub(super) fn set_file_drag(&self, drag: gtk::DragSource) {
        self.imp().file_drag.replace(Some(drag));
        self.sync_zoom_state();
    }

    pub(super) fn set_refine(&self, refine: Refine) {
        self.imp().refine.replace(Some(refine));
    }

    /// Shows `texture`, decoded to fit `decoded`, in place of the current one.
    pub(super) fn set_texture(&self, texture: gdk::Texture, decoded: ImagePreviewSize) {
        let imp = self.imp();
        imp.frame.imp().texture.replace(Some(texture));
        imp.decoded.set(Some(decoded));
        imp.frame.queue_draw();
        self.queue_allocate();
        self.schedule_refine();
    }

    pub(super) fn reset_zoom(&self) {
        self.zoom_at(self.view_center(), MIN_ZOOM);
    }

    pub(super) fn reset_if_zoomed(&self) -> bool {
        let zoomed = self.zoomed();
        if zoomed {
            self.reset_zoom();
        }
        zoomed
    }

    fn view_center(&self) -> (f64, f64) {
        (
            f64::from(self.width()) / 2.0,
            f64::from(self.height()) / 2.0,
        )
    }

    fn view_size(&self) -> (f64, f64) {
        (f64::from(self.width()), f64::from(self.height()))
    }

    fn texture_size(&self) -> Option<(u32, u32)> {
        let texture = self.imp().frame.imp().texture.borrow().clone()?;
        Some((texture.width() as u32, texture.height() as u32))
    }

    fn fit(&self) -> Option<(f64, f64)> {
        let texture = self.texture_size()?;
        let native = self
            .imp()
            .decoded
            .get()
            .is_some_and(|decoded| is_native(texture, decoded));
        Some(fit_size(
            self.view_size(),
            (f64::from(texture.0), f64::from(texture.1)),
            native,
        ))
    }

    fn placement(&self) -> Option<Placement> {
        let fit = self.fit()?;
        let imp = self.imp();
        Some(place(
            self.view_size(),
            fit,
            imp.zoom.get(),
            imp.center.get(),
        ))
    }

    fn zoomed(&self) -> bool {
        self.imp().zoom.get() > MIN_ZOOM
    }

    fn zoom_at(&self, anchor: (f64, f64), zoom: f64) {
        let Some(fit) = self.fit() else {
            return;
        };
        let imp = self.imp();
        let zoom = zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        let from = (imp.zoom.get(), imp.center.get());
        imp.center
            .set(anchored_center(self.view_size(), fit, from, zoom, anchor));
        imp.zoom.set(zoom);
        self.sync_zoom_state();
        self.queue_allocate();
        self.schedule_refine();
    }

    fn pan_to(&self, center: (f64, f64)) {
        let Some(fit) = self.fit() else {
            return;
        };
        let imp = self.imp();
        imp.center.set(settled_center(
            self.view_size(),
            fit,
            imp.zoom.get(),
            center,
        ));
        self.queue_allocate();
    }

    fn pan_by(&self, delta: (f64, f64)) {
        let Some(placement) = self.placement() else {
            return;
        };
        let center = self.imp().center.get();
        self.pan_to((
            center.0 + delta.0 / placement.width.max(1.0),
            center.1 + delta.1 / placement.height.max(1.0),
        ));
    }

    fn sync_zoom_state(&self) {
        let zoomed = self.zoomed();
        if let Some(drag) = self.imp().file_drag.borrow().as_ref() {
            drag.set_propagation_phase(if zoomed {
                gtk::PropagationPhase::None
            } else {
                gtk::PropagationPhase::Capture
            });
        }
        self.set_cursor_from_name(zoomed.then_some("grab"));
    }

    fn schedule_refine(&self) {
        let imp = self.imp();
        if let Some(timer) = imp.refine_timer.take() {
            timer.remove();
        }
        let weak = self.downgrade();
        imp.refine_timer.replace(Some(glib::timeout_add_local_once(
            REFINE_DELAY,
            move || {
                if let Some(view) = weak.upgrade() {
                    view.imp().refine_timer.take();
                    view.refine_now();
                }
            },
        )));
    }

    fn refine_now(&self) {
        let imp = self.imp();
        let (Some(texture), Some(decoded), Some(fit)) =
            (self.texture_size(), imp.decoded.get(), self.fit())
        else {
            return;
        };
        if self.width() <= 0 || self.height() <= 0 {
            return;
        }
        // One decode serves the whole zoom range rather than one per step.
        let reach = if self.zoomed() { MAX_ZOOM } else { MIN_ZOOM };
        let scale = f64::from(self.scale_factor()) * reach;
        let Some(next) = refinement(texture, decoded, (fit.0 * scale, fit.1 * scale)) else {
            return;
        };
        if imp.requested.get() == Some(next) {
            return;
        }
        imp.requested.set(Some(next));
        let refine = imp.refine.borrow().clone();
        if let Some(refine) = refine {
            refine(next);
        }
    }

    fn install_controllers(&self) {
        let motion = gtk::EventControllerMotion::new();
        let weak = self.downgrade();
        motion.connect_motion(move |_, x, y| {
            if let Some(view) = weak.upgrade() {
                view.imp().pointer.set((x, y));
            }
        });
        self.add_controller(motion);

        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
        let weak = self.downgrade();
        scroll.connect_scroll(move |controller, dx, dy| {
            let Some(view) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            if controller
                .current_event_state()
                .contains(gdk::ModifierType::CONTROL_MASK)
            {
                let zoom = view.imp().zoom.get() * (-dy * 0.14).exp();
                view.zoom_at(view.imp().pointer.get(), zoom);
                return glib::Propagation::Stop;
            }
            if !view.zoomed() {
                return glib::Propagation::Proceed;
            }
            let step = if controller.unit() == gdk::ScrollUnit::Wheel {
                WHEEL_PAN
            } else {
                1.0
            };
            view.pan_by((dx * step, dy * step));
            glib::Propagation::Stop
        });
        self.add_controller(scroll);

        let pinch = gtk::GestureZoom::new();
        let weak = self.downgrade();
        pinch.connect_begin(move |_, _| {
            if let Some(view) = weak.upgrade() {
                view.imp().pinch_start.set(view.imp().zoom.get());
            }
        });
        let weak = self.downgrade();
        pinch.connect_scale_changed(move |gesture, scale| {
            if let Some(view) = weak.upgrade() {
                let anchor = gesture
                    .bounding_box_center()
                    .unwrap_or_else(|| view.view_center());
                view.zoom_at(anchor, view.imp().pinch_start.get() * scale);
            }
        });
        self.add_controller(pinch);

        let pan = gtk::GestureDrag::new();
        let weak = self.downgrade();
        pan.connect_drag_begin(move |gesture, _, _| {
            let Some(view) = weak.upgrade() else {
                return;
            };
            if !view.zoomed() {
                gesture.set_state(gtk::EventSequenceState::Denied);
                return;
            }
            view.imp().pan_start.set(view.imp().center.get());
            view.set_cursor_from_name(Some("grabbing"));
        });
        let weak = self.downgrade();
        pan.connect_drag_update(move |_, x, y| {
            let Some(view) = weak.upgrade() else {
                return;
            };
            let Some(placement) = view.placement() else {
                return;
            };
            let start = view.imp().pan_start.get();
            view.pan_to((
                start.0 - x / placement.width.max(1.0),
                start.1 - y / placement.height.max(1.0),
            ));
        });
        let weak = self.downgrade();
        pan.connect_drag_end(move |_, _, _| {
            if let Some(view) = weak.upgrade() {
                view.sync_zoom_state();
            }
        });
        self.add_controller(pan);

        let click = gtk::GestureClick::new();
        let weak = self.downgrade();
        click.connect_pressed(move |_, presses, x, y| {
            let Some(view) = weak.upgrade() else {
                return;
            };
            if presses == 2 {
                if view.zoomed() {
                    view.reset_zoom();
                } else {
                    view.zoom_at((x, y), DOUBLE_CLICK_ZOOM);
                }
            }
        });
        self.add_controller(click);

        let reset = gio::SimpleAction::new("reset-zoom", None);
        let weak = self.downgrade();
        reset.connect_activate(move |_, _| {
            if let Some(view) = weak.upgrade() {
                view.reset_zoom();
            }
        });
        let actions = gio::SimpleActionGroup::new();
        actions.add_action(&reset);
        self.insert_action_group("image", Some(&actions));
    }
}

#[cfg(test)]
mod tests;
