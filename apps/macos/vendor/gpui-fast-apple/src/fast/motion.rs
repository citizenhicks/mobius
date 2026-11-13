use crate::metal_renderer::{InstanceBinding, InstanceBindings, InstanceBufferWriter};
use anyhow::Result;
use gpui::{MotionTransform, Scene};
use objc2::runtime::ProtocolObject;
use objc2_metal::MTLRenderCommandEncoder;
use std::time::Instant;
pub(crate) struct Bindings {
    pub(crate) buffer: InstanceBinding,
    pub(crate) transforms: Vec<MotionTransform>,
}
pub(crate) fn sample(scene: &Scene, writer: &mut InstanceBufferWriter) -> Result<Bindings> {
    let transforms = scene.motion_transforms(Instant::now());
    Ok(Bindings {
        buffer: writer.write(&transforms)?,
        transforms,
    })
}
pub(crate) fn color(
    path: &gpui::Path<gpui::ScaledPixels>,
    motions: &[MotionTransform],
) -> gpui::Background {
    path.color
        .opacity(motions[path.fast_motion as usize].opacity)
}
pub(crate) fn set_motions(
    command_encoder: &ProtocolObject<dyn MTLRenderCommandEncoder>,
    instance_bindings: &InstanceBindings,
    index: usize,
) {
    let motions = &instance_bindings.fast_motions.buffer;
    // SAFETY: The motion table was written by `write_instances` for this frame and is
    // read-only; Metal retains the buffer until the frame completes.
    unsafe {
        command_encoder.setVertexBuffer_offset_atIndex(
            Some(&motions.buffer),
            motions.offset,
            index,
        );
        command_encoder.setFragmentBuffer_offset_atIndex(
            Some(&motions.buffer),
            motions.offset,
            index,
        );
    }
}
pub(crate) fn moved(
    path: &gpui::Path<gpui::ScaledPixels>,
    motions: &[gpui::MotionTransform],
) -> (gpui::TransformationMatrix, gpui::Bounds<gpui::ScaledPixels>) {
    let transformation = motions
        .get(path.fast_motion as usize)
        .map_or(gpui::TransformationMatrix::unit(), |motion| {
            motion.transformation
        });
    if path.fast_motion == 0 {
        return (transformation, path.clipped_bounds());
    }
    let corners = [
        path.bounds.origin,
        path.bounds.top_right(),
        path.bounds.bottom_left(),
        path.bounds.bottom_right(),
    ]
    .map(|corner| apply_scaled(transformation, corner));
    let (min, max) = corners[1..]
        .iter()
        .fold((corners[0], corners[0]), |(min, max), corner| {
            (min.min(corner), max.max(corner))
        });
    let bounds = gpui::Bounds::from_corners(min, max).intersect(&path.content_mask.bounds);
    (transformation, bounds)
}
pub(crate) fn apply_scaled(
    transformation: gpui::TransformationMatrix,
    position: gpui::Point<gpui::ScaledPixels>,
) -> gpui::Point<gpui::ScaledPixels> {
    let position =
        transformation.apply(gpui::point(gpui::px(position.x.0), gpui::px(position.y.0)));
    gpui::point(
        gpui::ScaledPixels(f32::from(position.x)),
        gpui::ScaledPixels(f32::from(position.y)),
    )
}
