use crate::scene::PaintOperation;
use collections::FxHashMap;
use scheduler::Instant;
use std::cell::Cell;
use std::ops::Range;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::{
    AnyElement, App, Bounds, Element, ElementId, EntityId, GlobalElementId, Hsla,
    InspectorElementId, IntoElement, Pixels, Point, Primitive, Radians, ScaledPixels, Scene, Size,
    TransformationMatrix, Window, point, px, radians, size,
};

/// Appearance that changes over time without changing layout, sampled each time the
/// window presents a frame.
///
/// A window whose only changes are motions presents its last frame again with each
/// motion sampled anew, without rendering, laying out or painting any view. Primitives
/// painted while a motion is active move, rotate, scale and fade with it; nested motions
/// compose. Hit testing keeps using laid out bounds.
#[derive(Clone)]
pub struct Motion {
    sample: Rc<dyn Fn(Instant) -> MotionFrame>,
    until: Option<Rc<Cell<Instant>>>,
    anchor: (f32, f32),
    interval: Option<Duration>,
    fixed_geometry: bool,
}

impl Motion {
    /// A motion sampled from `sample` for as long as it stays painted.
    pub fn new(sample: impl Fn(Instant) -> MotionFrame + 'static) -> Self {
        Self {
            sample: Rc::new(sample),
            until: None,
            anchor: (0.5, 0.5),
            interval: None,
            fixed_geometry: false,
        }
    }

    /// Fade without moving geometry, preserving ordinary clipping and batching.
    pub fn opacity(sample: impl Fn(Instant) -> f32 + 'static) -> Self {
        let mut motion = Self::new(move |at| MotionFrame {
            opacity: sample(at),
            ..MotionFrame::default()
        });
        motion.fixed_geometry = true;
        motion
    }

    /// Present at most `fps` frames a second for this motion, rather than one every
    /// display refresh. A window presents for its most frequent active motion.
    /// This caps presentation, not display-link callbacks.
    pub fn max_fps(mut self, fps: f32) -> Self {
        self.interval = (fps.is_finite() && fps > 0.)
            .then(|| Duration::try_from_secs_f64(1. / f64::from(fps)).unwrap_or(Duration::MAX));
        self
    }

    /// Where in an element's bounds [`MotionExt::with_motion`] rotates and scales it
    /// around, from (0, 0) at the top left to (1, 1) at the bottom right; the centre by
    /// default.
    pub fn anchor(mut self, x: f32, y: f32) -> Self {
        self.anchor = (x, y);
        self
    }

    /// Stop presenting new frames for this motion after `end`; it keeps its last sample.
    pub fn until(self, end: Instant) -> Self {
        self.until_cell(Rc::new(Cell::new(end)))
    }

    /// [`Self::until`] an end its owner can move after the motion is painted, for motion
    /// that changes course without the view rendering again: the sample closure reads
    /// the new course from state it shares, and [`Window::refresh_motions`] presents it.
    pub fn until_cell(mut self, end: Rc<Cell<Instant>>) -> Self {
        self.until = Some(end);
        self
    }

    /// The motion's appearance at `at`.
    fn sample(&self, at: Instant) -> MotionFrame {
        let end = self.until.as_ref().map_or(at, |end| end.get());
        (self.sample)(at.min(end))
    }

    /// Whether the motion still changes at `at`.
    fn is_active(&self, at: Instant) -> bool {
        self.until.as_ref().is_none_or(|end| at < end.get())
    }
}

/// A motion's appearance at one instant, relative to where its content was laid out.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MotionFrame {
    /// Movement from the laid out position.
    pub offset: Point<Pixels>,
    /// Clockwise rotation around the motion's origin.
    pub rotation: Radians,
    /// Scale around the motion's origin.
    pub scale: Size<f32>,
    /// Multiplies the opacity of everything painted with the motion.
    pub opacity: f32,
    /// A band brightening glyphs as it passes over them.
    pub highlight: Option<MotionHighlight>,
}

impl Default for MotionFrame {
    fn default() -> Self {
        Self {
            offset: point(px(0.), px(0.)),
            rotation: radians(0.),
            scale: size(1., 1.),
            opacity: 1.,
            highlight: None,
        }
    }
}

/// Bands of `color` stacked `layers` deep at `center` and thinning out to nothing at
/// `radius` on either side, composited over monochrome glyphs: a sweep or shimmer whose
/// position is sampled each frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MotionHighlight {
    /// Horizontal window position of the band's centre.
    pub center: Pixels,
    /// Half the width of the widest band.
    pub radius: Pixels,
    /// Color of each band; its alpha is the opacity of a single band.
    pub color: Hsla,
    /// Number of nested bands.
    pub layers: u32,
}

/// Extension trait for attaching a [`Motion`] to an element.
pub trait MotionExt {
    /// Paint this element with `motion`, around its [`Motion::anchor`]. Under
    /// [`App::reduce_motion`] the element is painted as laid out.
    fn with_motion(self, motion: Motion) -> MotionElement<Self>
    where
        Self: Sized,
    {
        MotionElement {
            element: Some(self),
            fast_motion: motion,
        }
    }
}

impl<E: IntoElement + 'static> MotionExt for E {}

/// An element painted with a [`Motion`]. See [`MotionExt::with_motion`].
pub struct MotionElement<E> {
    element: Option<E>,
    fast_motion: Motion,
}

impl<E: IntoElement + 'static> IntoElement for MotionElement<E> {
    type Element = MotionElement<E>;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl<E: IntoElement + 'static> Element for MotionElement<E> {
    type RequestLayoutState = AnyElement;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (crate::LayoutId, Self::RequestLayoutState) {
        let mut element = self
            .element
            .take()
            .expect("should only be called once")
            .into_any_element();
        (element.request_layout(window, cx), element)
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        element: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        element.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        element: &mut Self::RequestLayoutState,
        _: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        if cx.reduce_motion() {
            element.paint(window, cx);
        } else {
            let (x, y) = self.fast_motion.anchor;
            let origin = bounds.origin + point(bounds.size.width * x, bounds.size.height * y);
            window.with_motion(self.fast_motion.clone(), origin, |window| {
                element.paint(window, cx)
            });
        }
    }
}

/// A [`Motion`] as painted into a [`Scene`].
#[derive(Clone)]
pub(crate) struct SceneMotion {
    pub fast_motion: Motion,
    scope: Scope,
    owner: Option<EntityId>,
    painted: bool,
    /// The enclosing motion's index plus one, or zero.
    pub parent: u32,
    /// The point the motion rotates and scales around.
    pub origin: Point<ScaledPixels>,
    pub scale_factor: f32,
}

/// A motion sampled for one frame, composed with its enclosing motions, as read by shaders.
#[derive(Clone, Copy, Debug)]
#[repr(C)]
#[expect(missing_docs)]
pub struct MotionTransform {
    pub transformation: TransformationMatrix,
    pub opacity: f32,
    pub highlight_center: f32,
    pub highlight_radius: f32,
    /// Zero when the motion has no highlight.
    pub highlight_layers: u32,
    pub highlight_color: Hsla,
}

impl Default for MotionTransform {
    fn default() -> Self {
        Self {
            transformation: TransformationMatrix::unit(),
            opacity: 1.,
            highlight_center: 0.,
            highlight_radius: 0.,
            highlight_layers: 0,
            highlight_color: Hsla::default(),
        }
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Scope(u64);
impl Scope {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        assert_ne!(id, 0, "motion scope identity exhausted");
        Self(id)
    }
    pub(crate) fn rebased(&self, from: &Self, to: &Self) -> Self {
        if self == from { *to } else { *self }
    }
}
pub(crate) fn scope(scene: &Scene) -> Scope {
    scene
        .fast_motions
        .stack
        .last()
        .map_or_else(Scope::default, |index| {
            scene.fast_motions.entries[*index as usize - 1].scope
        })
}

#[derive(Default, Clone)]
pub(crate) struct SceneMotions {
    pub(crate) entries: Rc<Vec<SceneMotion>>,
    stack: Vec<u32>,
}

#[derive(Default)]
pub(crate) struct MotionClock {
    last_present: Cell<Option<Instant>>,
}

pub(crate) fn current(scene: &Scene) -> u32 {
    scene.fast_motions.stack.last().copied().unwrap_or(0)
}
pub(crate) fn clear(scene: &mut Scene) {
    if let Some(entries) = Rc::get_mut(&mut scene.fast_motions.entries) {
        entries.clear();
    } else {
        scene.fast_motions.entries = Rc::default();
    }
    scene.fast_motions.stack.clear();
}
fn primitive_motion(primitive: &mut Primitive) -> Option<&mut u32> {
    match primitive {
        Primitive::Shadow(p) => Some(&mut p.pad),
        Primitive::Quad(p) => Some(&mut p.fast_motion),
        Primitive::Path(p) => Some(&mut p.fast_motion),
        Primitive::Underline(p) => Some(&mut p.pad),
        Primitive::MonochromeSprite(p) => Some(&mut p.pad),
        Primitive::PolychromeSprite(p) => Some(&mut p.fast_motion),
        Primitive::SubpixelSprite(_) | Primitive::Surface(_) => None,
    }
}
pub(crate) fn stamp(scene: &Scene, primitive: &mut Primitive) {
    if let Some(motion) = primitive_motion(primitive) {
        *motion = current(scene);
    }
}
pub(crate) fn remap_primitive(
    scene: &mut Scene,
    previous: &Scene,
    primitive: &mut Primitive,
    replayed: &mut FxHashMap<u32, u32>,
    previous_parent: &Scope,
) {
    if let Some(motion) = primitive_motion(primitive) {
        *motion = scene.replay_motion(*motion, previous, replayed, previous_parent);
    }
}
pub(crate) fn has_motion(scene: &Scene) -> bool {
    scene.fast_motions.entries.iter().any(|entry| entry.painted)
}
pub(crate) fn replay_primitive(
    scene: &mut Scene,
    previous: &Scene,
    primitive: &Primitive,
    replayed: &mut FxHashMap<u32, u32>,
    previous_parent: &Scope,
) {
    let mut primitive = primitive.clone();
    remap_primitive(scene, previous, &mut primitive, replayed, previous_parent);
    scene.insert(primitive);
}
pub(crate) fn reuse(window: &mut Window, range: Range<usize>, parent: &Scope) {
    let before = window.next_frame.scene.fast_motions.entries.len();
    replay(
        &mut window.next_frame.scene,
        range,
        &window.rendered_frame.scene,
        parent,
    );
    let mut last_owner = None;
    for entry in &window.next_frame.scene.fast_motions.entries[before..] {
        if entry.painted
            && let Some(owner) = entry.owner
            && Some(owner) != last_owner
        {
            crate::fast::layers::invalidate::note_animation_frame(window, owner);
            last_owner = Some(owner);
        }
    }
}
pub(crate) fn replay(
    scene: &mut Scene,
    range: Range<usize>,
    previous: &Scene,
    previous_parent: &Scope,
) {
    crate::fast::layers::paint::replay_layers(scene, range.clone(), previous);
    let mut replayed = FxHashMap::default();
    for operation in &previous.paint_operations[range] {
        match operation {
            PaintOperation::Primitive(primitive) => {
                replay_primitive(scene, previous, primitive, &mut replayed, previous_parent)
            }
            PaintOperation::StartLayer(bounds) => scene.push_layer(*bounds),
            PaintOperation::EndLayer => scene.pop_layer(),
            PaintOperation::FastRoundedClip(clip) => {
                crate::fast::rounded_clip::record(scene, *clip)
            }
        }
    }
}
fn interval(scene: &Scene, now: Instant, last: Option<Instant>) -> Option<Duration> {
    let mut shortest: Option<Duration> = None;
    for entry in scene
        .fast_motions
        .entries
        .iter()
        .filter(|entry| entry.painted)
    {
        let next = if entry.fast_motion.is_active(now) {
            entry.fast_motion.interval.unwrap_or_default()
        } else if entry
            .fast_motion
            .until
            .as_ref()
            .is_some_and(|end| last.is_none_or(|last| last < end.get()))
        {
            Duration::ZERO
        } else {
            continue;
        };
        if next.is_zero() {
            return Some(next);
        }
        shortest = Some(shortest.map_or(next, |previous| previous.min(next)));
    }
    shortest
}
impl MotionClock {
    fn due(&self, scene: &Scene, now: Instant) -> bool {
        interval(scene, now, self.last_present.get()).is_some_and(|interval| {
            self.last_present.get().is_none_or(|last| {
                now.saturating_duration_since(last)
                    .saturating_add(Duration::from_millis(2))
                    >= interval
            })
        })
    }
}
pub(crate) fn is_animating(window: &Window) -> bool {
    interval(
        &window.rendered_frame.scene,
        Instant::now(),
        window.fast_motion.last_present.get(),
    )
    .is_some()
}
// A motion-only frame re-presents the retained scene at its fastest active rate.
pub(crate) fn due(window: &Window) -> bool {
    window
        .fast_motion
        .due(&window.rendered_frame.scene, Instant::now())
}
pub(crate) fn presented(window: &Window) {
    window.fast_motion.last_present.set(Some(Instant::now()));
}

impl Scene {
    pub(crate) fn push_motion(
        &mut self,
        motion: Motion,
        origin: Point<ScaledPixels>,
        scale_factor: f32,
        owner: Option<EntityId>,
    ) {
        let parent = current(self);
        Rc::make_mut(&mut self.fast_motions.entries).push(SceneMotion {
            fast_motion: motion,
            scope: Scope::new(),
            owner,
            painted: false,
            parent,
            origin,
            scale_factor,
        });
        self.fast_motions
            .stack
            .push(self.fast_motions.entries.len() as u32);
    }
    pub(crate) fn pop_motion(&mut self) {
        self.fast_motions.stack.pop();
    }
    /// Sample the renderer ABI table; entry zero is the identity.
    pub fn motion_transforms(&self, at: Instant) -> Vec<MotionTransform> {
        let mut transforms = Vec::with_capacity(self.fast_motions.entries.len() + 1);
        transforms.push(MotionTransform::default());
        for motion in self.fast_motions.entries.iter() {
            if !motion.painted {
                transforms.push(MotionTransform::default());
                continue;
            }
            let frame = motion.fast_motion.sample(at);
            let scale = motion.scale_factor;
            let origin = motion.origin;
            let local = TransformationMatrix::unit()
                .translate(origin + frame.offset.scale(scale))
                .rotate(frame.rotation)
                .scale(frame.scale)
                .translate(point(ScaledPixels(-origin.x.0), ScaledPixels(-origin.y.0)));
            // Parents precede their children, so theirs are already composed.
            let parent = transforms[motion.parent as usize];
            transforms.push(match frame.highlight {
                Some(highlight) => MotionTransform {
                    transformation: parent.transformation.compose(local),
                    opacity: parent.opacity * frame.opacity,
                    highlight_center: highlight.center.scale(scale).0,
                    highlight_radius: highlight.radius.scale(scale).0,
                    highlight_layers: highlight.layers,
                    highlight_color: highlight.color,
                },
                None => MotionTransform {
                    transformation: parent.transformation.compose(local),
                    opacity: parent.opacity * frame.opacity,
                    ..parent
                },
            });
        }
        transforms
    }
    fn replay_motion(
        &mut self,
        index: u32,
        prev_scene: &Scene,
        replayed: &mut FxHashMap<u32, u32>,
        previous_parent: &Scope,
    ) -> u32 {
        if index == 0
            || prev_scene.fast_motions.entries[index as usize - 1].scope == *previous_parent
        {
            return current(self);
        }
        if let Some(index) = replayed.get(&index) {
            return *index;
        }
        let mut motion = prev_scene.fast_motions.entries[index as usize - 1].clone();
        motion.parent = self.replay_motion(motion.parent, prev_scene, replayed, previous_parent);
        motion.painted = false;
        Rc::make_mut(&mut self.fast_motions.entries).push(motion);
        let replayed_index = self.fast_motions.entries.len() as u32;
        replayed.insert(index, replayed_index);
        replayed_index
    }
}
impl Window {
    /// Present a changed motion course without rendering views.
    pub fn refresh_motions(&self) {
        self.needs_present.set(true);
        self.platform_window.schedule_frame();
    }
    /// Paint primitives under a motion; hit testing remains at laid-out bounds.
    pub fn with_motion<R>(
        &mut self,
        motion: Motion,
        origin: Point<Pixels>,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        self.invalidator.debug_assert_paint();

        let scale_factor = self.scale_factor();
        let owner = self.current_view();
        self.next_frame.scene.push_motion(
            motion,
            origin.scale(scale_factor),
            scale_factor,
            Some(owner),
        );
        let index = current(&self.next_frame.scene);
        let result = f(self);
        self.next_frame.scene.pop_motion();
        if self.next_frame.scene.fast_motions.entries[index as usize - 1].painted {
            crate::fast::layers::invalidate::note_animation_frame(self, owner);
        }
        result
    }
}

pub(crate) fn bounds(scene: &mut Scene, primitive: &mut Primitive) -> Bounds<ScaledPixels> {
    let mut index = primitive_motion(primitive).copied().unwrap_or(0);
    if index == 0 {
        return primitive
            .bounds()
            .intersect(&primitive.content_mask().bounds);
    }
    let mut bounds = primitive
        .bounds()
        .intersect(&primitive.content_mask().bounds);
    let mut parent = index;
    while parent != 0 {
        let motion = &scene.fast_motions.entries[parent as usize - 1];
        if !motion.fast_motion.fixed_geometry {
            // An arbitrary ancestor can enter anywhere in this mask.
            bounds = primitive.content_mask().bounds;
            break;
        }
        parent = motion.parent;
    }
    if !bounds.is_empty() {
        while index != 0 && !scene.fast_motions.entries[index as usize - 1].painted {
            let entry = &mut Rc::make_mut(&mut scene.fast_motions.entries)[index as usize - 1];
            entry.painted = true;
            index = entry.parent;
        }
    }
    bounds
}

/// Run retained-motion, clipping, FPS and final-frame invariants against the compiled core.
#[cfg(any(test, feature = "test-support"))]
pub fn assert_motion_invariants() {
    tests::retained_paint_rebases_outer_motion_and_keeps_inner_motion();
    tests::fps_bounds_and_final_sample_do_not_panic_or_leave_a_motion_unfinished();
}

#[cfg(any(test, feature = "test-support"))]
mod tests {
    use super::{Motion, MotionClock, MotionFrame, Scope, interval, replay, scope};
    use crate::{Bounds, ContentMask, Quad, ScaledPixels, Scene, point, px, size};
    use scheduler::Instant;
    use std::time::Duration;

    fn translated(x: f32) -> Motion {
        Motion::new(move |_| MotionFrame {
            offset: point(px(x), px(0.)),
            ..MotionFrame::default()
        })
    }
    fn quad() -> Quad {
        let bounds = Bounds::new(
            point(ScaledPixels(0.), ScaledPixels(0.)),
            size(ScaledPixels(10.), ScaledPixels(10.)),
        );
        Quad {
            bounds,
            content_mask: ContentMask { bounds },
            ..Quad::default()
        }
    }
    fn push(scene: &mut Scene, motion: Motion) {
        scene.push_motion(motion, point(ScaledPixels(0.), ScaledPixels(0.)), 1., None);
    }
    fn position(scene: &Scene, at: Instant) -> f32 {
        f32::from(
            scene.motion_transforms(at)[scene.quads[0].fast_motion as usize]
                .transformation
                .apply(point(px(0.), px(0.)))
                .x,
        )
    }
    #[cfg_attr(test, test)]
    pub(super) fn retained_paint_rebases_outer_motion_and_keeps_inner_motion() {
        let now = Instant::now();
        let mut previous = Scene::default();
        push(&mut previous, translated(1.));
        let old_scope = scope(&previous);
        push(&mut previous, translated(4.));
        previous.insert_primitive(quad());
        previous.pop_motion();
        previous.pop_motion();
        let mut retained = Scene::default();
        push(&mut retained, translated(10.));
        replay(&mut retained, 0..previous.len(), &previous, &old_scope);
        retained.pop_motion();
        let mut fresh = Scene::default();
        push(&mut fresh, translated(10.));
        push(&mut fresh, translated(4.));
        fresh.insert_primitive(quad());
        fresh.pop_motion();
        fresh.pop_motion();
        assert_eq!(position(&retained, now), position(&fresh, now));
        assert_eq!(position(&retained, now), 14.);
        let shifted = old_scope.rebased(&old_scope, &Scope::default());
        assert!(
            shifted == Scope::default(),
            "retained indices must follow their new enclosing scope"
        );
        let plain = {
            let mut scene = Scene::default();
            scene.insert_primitive(quad());
            scene
        };
        let mut added = Scene::default();
        push(&mut added, translated(10.));
        added.replay(0..plain.len(), &plain);
        assert_eq!(position(&added, now), 10.);
        let mut composed = Scene::default();
        composed
            .replay_balanced(0..previous.len(), &previous)
            .unwrap();
        assert_eq!(position(&composed, now), 5.);
        let shared = translated(3.);
        let mut nested = Scene::default();
        push(&mut nested, shared.clone());
        let outer = scope(&nested);
        push(&mut nested, shared);
        nested.insert_primitive(quad());
        let mut rebased = Scene::default();
        push(&mut rebased, translated(10.));
        replay(&mut rebased, 0..nested.len(), &nested, &outer);
        assert_eq!(
            position(&rebased, now),
            13.,
            "cloned samplers must remain distinct nested scopes"
        );
        let mut entering = Scene::default();
        push(&mut entering, translated(20.));
        let mut outside = quad();
        outside.bounds.origin.x = ScaledPixels(-20.);
        entering.insert_primitive(outside);
        assert_eq!(
            entering.quads.len(),
            1,
            "moving content must not be culled at its initial position"
        );
    }
    #[cfg_attr(test, test)]
    pub(super) fn fps_bounds_and_final_sample_do_not_panic_or_leave_a_motion_unfinished() {
        assert_eq!(
            translated(0.).max_fps(f32::from_bits(1)).interval,
            Some(Duration::MAX)
        );
        assert_eq!(
            translated(0.).max_fps(60.).interval,
            Some(Duration::from_secs_f64(1. / 60.))
        );
        for fps in [0., -1., f32::NAN, f32::INFINITY] {
            assert_eq!(translated(0.).max_fps(fps).interval, None);
        }
        let start = Instant::now();
        let mut fade = Scene::default();
        push(&mut fade, Motion::opacity(|_| 0.5));
        let mut outside = quad();
        outside.bounds.origin.x = ScaledPixels(-20.);
        fade.insert_primitive(outside.clone());
        assert!(
            fade.quads.is_empty(),
            "opacity-only content outside the mask must be culled"
        );
        assert_eq!(interval(&fade, start, None), None);
        fade.insert_primitive(quad());
        assert_eq!(fade.motion_transforms(start)[1].opacity, 0.5);
        let mut entering = Scene::default();
        push(&mut entering, translated(20.));
        push(&mut entering, Motion::opacity(|_| 0.5));
        entering.insert_primitive(outside);
        assert_eq!(
            entering.quads.len(),
            1,
            "a moving ancestor must preserve entering opacity-only content"
        );
        assert_eq!(position(&entering, start), 20.);
        let end = start + Duration::from_millis(10);
        let mut scene = Scene::default();
        push(
            &mut scene,
            Motion::new(move |at| MotionFrame {
                opacity: if at == end { 1. } else { 0. },
                ..MotionFrame::default()
            })
            .until(end)
            .max_fps(30.),
        );
        assert_eq!(
            interval(&scene, start, None),
            None,
            "an empty motion must not drive frames"
        );
        let mut clipped = quad();
        clipped.content_mask.bounds.size.width = ScaledPixels(0.);
        scene.insert_primitive(clipped);
        assert_eq!(
            interval(&scene, start, None),
            None,
            "a fully clipped motion must not drive frames"
        );
        scene.insert_primitive(quad());
        let after = end + Duration::from_millis(2);
        assert_eq!(interval(&scene, after, Some(start)), Some(Duration::ZERO));
        assert_eq!(scene.motion_transforms(after)[1].opacity, 1.);
        assert_eq!(interval(&scene, after, Some(after)), None);
        let mut cadence = Scene::default();
        push(&mut cadence, translated(0.).max_fps(30.));
        cadence.insert_primitive(quad());
        let clock = MotionClock::default();
        assert!(clock.due(&cadence, start));
        clock.last_present.set(Some(start));
        assert!(!clock.due(&cadence, start + Duration::from_millis(30)));
        assert!(clock.due(&cadence, start + Duration::from_millis(32)));
        assert!(
            clock.due(&scene, after),
            "completion cannot be skipped by the fps gate"
        );
        clock.last_present.set(Some(after));
        assert!(
            !clock.due(&scene, after),
            "a completed final frame must stop"
        );
    }
}
