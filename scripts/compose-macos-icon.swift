import AppKit
import Foundation

let canvasSide: CGFloat = 1024
let tileInset: CGFloat = 72
let tileCornerRadius: CGFloat = 208
let artworkInset: CGFloat = 34

guard CommandLine.arguments.count == 3 else {
    fputs("usage: compose-macos-icon.swift SOURCE_SQUARE_PNG OUTPUT_PNG\n", stderr)
    exit(2)
}

let sourceURL = URL(fileURLWithPath: CommandLine.arguments[1])
let outputURL = URL(fileURLWithPath: CommandLine.arguments[2])

guard let source = NSImage(contentsOf: sourceURL), source.size.width == source.size.height else {
    fputs("source image must be a readable square PNG\n", stderr)
    exit(2)
}

guard let bitmap = NSBitmapImageRep(
    bitmapDataPlanes: nil,
    pixelsWide: Int(canvasSide),
    pixelsHigh: Int(canvasSide),
    bitsPerSample: 8,
    samplesPerPixel: 4,
    hasAlpha: true,
    isPlanar: false,
    colorSpaceName: .deviceRGB,
    bytesPerRow: 0,
    bitsPerPixel: 0
), let graphics = NSGraphicsContext(bitmapImageRep: bitmap) else {
    fputs("could not create icon canvas\n", stderr)
    exit(1)
}

let canvas = NSRect(x: 0, y: 0, width: canvasSide, height: canvasSide)
let tile = canvas.insetBy(dx: tileInset, dy: tileInset)
let artwork = tile.insetBy(dx: artworkInset, dy: artworkInset)

NSGraphicsContext.saveGraphicsState()
NSGraphicsContext.current = graphics
graphics.shouldAntialias = true
graphics.imageInterpolation = .high
graphics.cgContext.clear(canvas)

let silhouette = NSBezierPath(
    roundedRect: tile,
    xRadius: tileCornerRadius,
    yRadius: tileCornerRadius
)
silhouette.addClip()
NSColor.black.setFill()
silhouette.fill()
source.draw(
    in: artwork,
    from: NSRect(origin: .zero, size: source.size),
    operation: .sourceOver,
    fraction: 1,
    respectFlipped: true,
    hints: [.interpolation: NSImageInterpolation.high]
)
NSGraphicsContext.restoreGraphicsState()

try FileManager.default.createDirectory(
    at: outputURL.deletingLastPathComponent(),
    withIntermediateDirectories: true
)
guard let png = bitmap.representation(using: .png, properties: [:]) else {
    fputs("could not encode icon PNG\n", stderr)
    exit(1)
}
try png.write(to: outputURL, options: .atomic)
