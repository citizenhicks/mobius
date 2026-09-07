import Foundation
import SwiftUI

/// The native 64-point Composing preset from thinking-orbs 0.2.0.
///
/// möbius only needs this branded state, so the React/canvas package is not embedded. The
/// geometry and timing stay here with the Apple drawing primitive; the MIT notice is in NOTICE.
struct MobiusComposingOrb: View {
    @Environment(\.colorScheme) private var colorScheme
    @Environment(\.accessibilityReduceMotion) private var reduceMotion
    @Environment(\.scenePhase) private var scenePhase

    var body: some View {
        TimelineView(.animation(
            minimumInterval: 1.0 / 30.0,
            paused: reduceMotion || scenePhase != .active
        )) { _ in
            let seconds = ProcessInfo.processInfo.systemUptime
            let time = reduceMotion || scenePhase != .active
                ? 0.6
                : seconds * MobiusComposingOrbRenderer.speed
            Canvas(rendersAsynchronously: true) { [colorScheme, time] context, size in
                let sourceSize = CGFloat(MobiusComposingOrbRenderer.size)
                let scale = min(size.width, size.height) / sourceSize
                context.translateBy(
                    x: (size.width - sourceSize * scale) / 2,
                    y: (size.height - sourceSize * scale) / 2
                )
                context.scaleBy(x: scale, y: scale)

                for dot in MobiusComposingOrbRenderer.dots(at: time) {
                    let radius = max(0.3, dot.radius)
                    let rect = CGRect(
                        x: dot.x - radius,
                        y: dot.y - radius,
                        width: radius * 2,
                        height: radius * 2
                    )
                    context.fill(
                        Path(ellipseIn: rect),
                        with: .color(
                            MobiusPalette.composingOrbInk(white: dot.white, scheme: colorScheme)
                                .opacity(dot.opacity)
                        )
                    )
                }
            }
        }
    }
}

enum MobiusComposingOrbRenderer {
    struct Dot: Equatable {
        let x: Double
        let y: Double
        let z: Double
        let radius: Double
        let white: Double
        let opacity: Double
    }

    static let size = 64.0
    static let speed = 2.34

    static func dots(at time: Double) -> [Dot] {
        let center = size / 2
        let sphereRadius = size / 2 * 0.78
        let radiusScale = pow(size / 300, 0.6)
        var dots: [Dot] = []
        dots.reserveCapacity(566)

        let ghostCount = 38
        let goldenAngle = Double.pi * (3 - sqrt(5))
        for index in 0..<ghostCount {
            let y = 1 - 2 * (Double(index) + 0.5) / Double(ghostCount)
            let radial = sqrt(1 - y * y)
            let angle = Double(index) * goldenAngle
            let projected = project(
                x: radial * cos(angle) * sphereRadius,
                y: y * sphereRadius,
                z: radial * sin(angle) * sphereRadius,
                center: center
            )
            let depth = (projected.z / sphereRadius + 1) / 2
            dots.append(Dot(
                x: projected.x,
                y: projected.y,
                z: projected.z,
                radius: 0.8 * radiusScale,
                white: 0.78,
                opacity: 0.1 + 0.22 * depth
            ))
        }

        let tilt = 0.55
        let sinTilt = sin(tilt)
        let cosTilt = cos(tilt)
        let laneCount = 12
        let segmentCount = 44
        for lane in 0..<laneCount {
            let laneOffset = (Double(lane) - Double(laneCount - 1) / 2) * 0.075
            let edge = abs(Double(lane) - Double(laneCount - 1) / 2)
                / max(1, Double(laneCount - 1) / 2)

            for segment in 0..<segmentCount {
                let angle = Double(segment) / Double(segmentCount) * 2 * .pi
                let wobble = 0.16 * sin(angle * 3 - time * 1.7 + Double(lane) * 0.22)
                    + 0.07 * sin(angle * 5 + time * 1.1)
                let offset = laneOffset + wobble
                let x = cos(angle)
                let y = cosTilt * sin(angle) - sinTilt * offset
                let z = sinTilt * sin(angle) + cosTilt * offset
                let length = sqrt(x * x + y * y + z * z)
                let projected = project(
                    x: x / length * sphereRadius,
                    y: y / length * sphereRadius,
                    z: z / length * sphereRadius,
                    center: center
                )
                let depth = (projected.z / sphereRadius + 1) / 2
                dots.append(Dot(
                    x: projected.x,
                    y: projected.y,
                    z: projected.z,
                    radius: (0.935 + 1.445 * depth) * (1 - 0.25 * edge) * radiusScale,
                    white: 0.52 - 0.44 * depth + 0.18 * edge,
                    opacity: 0.4 + 0.6 * depth
                ))
            }
        }

        return dots.sorted { $0.z < $1.z }
    }

    private static func project(
        x: Double,
        y: Double,
        z: Double,
        center: Double
    ) -> (x: Double, y: Double, z: Double) {
        let sinTilt = sin(0.3)
        let cosTilt = cos(0.3)
        let projectedY = y * cosTilt - z * sinTilt
        return (
            center + x,
            center - projectedY,
            y * sinTilt + z * cosTilt
        )
    }
}
