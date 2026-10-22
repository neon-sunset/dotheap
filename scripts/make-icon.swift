// Draws the app icon: a grid of heap regions filled to different levels, one of them selected as on the region map.
// usage: swift scripts/make-icon.swift <out.iconset folder>; then iconutil -c icns <folder>
import AppKit

let out = CommandLine.arguments.count > 1 ? CommandLine.arguments[1] : "Dotheap.iconset"
try? FileManager.default.createDirectory(atPath: out, withIntermediateDirectories: true)

func color(_ hex: UInt32, _ alpha: CGFloat = 1) -> CGColor {
    CGColor(srgbRed: CGFloat(hex >> 16 & 0xFF) / 255, green: CGFloat(hex >> 8 & 0xFF) / 255, blue: CGFloat(hex & 0xFF) / 255, alpha: alpha)
}

func draw(_ ctx: CGContext, size: CGFloat) {
    let s = size / 1024
    // The macOS icon grid: an 824-point tile with a 185-point corner radius, centered.
    let tile = CGRect(x: 100 * s, y: 100 * s, width: 824 * s, height: 824 * s)
    let tilePath = CGPath(roundedRect: tile, cornerWidth: 185 * s, cornerHeight: 185 * s, transform: nil)
    ctx.saveGState()
    ctx.setShadow(offset: CGSize(width: 0, height: -10 * s), blur: 28 * s, color: color(0x000000, 0.35))
    ctx.addPath(tilePath)
    ctx.setFillColor(color(0x1B1F3B))
    ctx.fillPath()
    ctx.restoreGState()
    ctx.saveGState()
    ctx.addPath(tilePath)
    ctx.clip()
    let gradient = CGGradient(colorsSpace: CGColorSpace(name: CGColorSpace.sRGB), colors: [color(0x2A2F5E), color(0x14172E)] as CFArray, locations: [0, 1])!
    ctx.drawLinearGradient(gradient, start: CGPoint(x: tile.midX, y: tile.maxY), end: CGPoint(x: tile.midX, y: tile.minY), options: [])

    // Light from the top left of the tile.
    let light = CGGradient(colorsSpace: CGColorSpace(name: CGColorSpace.sRGB), colors: [color(0xFFFFFF, 0.10), color(0xFFFFFF, 0)] as CFArray, locations: [0, 1])!
    ctx.drawRadialGradient(light, startCenter: CGPoint(x: tile.minX + 180 * s, y: tile.maxY - 120 * s), startRadius: 0,
                           endCenter: CGPoint(x: tile.minX + 180 * s, y: tile.maxY - 120 * s), endRadius: 760 * s, options: [])

    // 4 x 4 regions as in the region map's Bytes view: live, pending finalization and dead bytes from the bottom up.
    let regions: [[(live: CGFloat, pending: CGFloat, dead: CGFloat)]] = [
        [(0.92, 0, 0), (0.70, 0, 0.12), (0.28, 0, 0.10), (0.86, 0, 0)],
        [(0.55, 0, 0.10), (0.18, 0.14, 0.48), (0.84, 0, 0), (0.12, 0, 0.06)],
        [(0.88, 0, 0), (0.40, 0, 0.22), (0.96, 0, 0), (0.22, 0.10, 0.30)],
        [(0.16, 0, 0.08), (0.74, 0, 0), (0.50, 0.12, 0.14), (0.82, 0, 0)],
    ]
    let colors: [(top: UInt32, bottom: UInt32)] = [(0x45C784, 0x2A9E5C), (0xE6C64A, 0xC9A62C), (0xF59A52, 0xDE6E26)]
    let grid = tile.insetBy(dx: 118 * s, dy: 118 * s)
    let gap = 26 * s
    let cell = (grid.width - 3 * gap) / 4
    let space = CGColorSpace(name: CGColorSpace.sRGB)
    let selectedRow = 1, selectedColumn = 2
    func cellPath(_ row: Int, _ column: Int) -> (CGRect, CGPath) {
        let rect = CGRect(x: grid.minX + CGFloat(column) * (cell + gap), y: grid.maxY - CGFloat(row + 1) * cell - CGFloat(row) * gap, width: cell, height: cell)
        return (rect, CGPath(roundedRect: rect, cornerWidth: 26 * s, cornerHeight: 26 * s, transform: nil))
    }
    // The selected region's glow, under all regions.
    for _ in 0..<2 {
        ctx.saveGState()
        ctx.setShadow(offset: .zero, blur: 70 * s, color: color(0x0A84FF))
        ctx.addPath(cellPath(selectedRow, selectedColumn).1)
        ctx.setFillColor(color(0x0A84FF))
        ctx.fillPath()
        ctx.restoreGState()
    }
    for row in 0..<4 {
        for column in 0..<4 {
            let (rect, path) = cellPath(row, column)
            let selected = row == selectedRow && column == selectedColumn
            ctx.addPath(path)
            ctx.setFillColor(color(0x161A36))
            ctx.fillPath()
            ctx.addPath(path)
            ctx.setFillColor(color(0xFFFFFF, 0.09))
            ctx.fillPath()
            ctx.saveGState()
            ctx.addPath(path)
            ctx.clip()
            let r = regions[row][column]
            var y = rect.minY
            for (k, share) in [r.live, r.pending, r.dead].enumerated() where share > 0 {
                let part = CGRect(x: rect.minX, y: y, width: rect.width, height: rect.height * share)
                let shade = CGGradient(colorsSpace: space, colors: [color(colors[k].top), color(colors[k].bottom)] as CFArray, locations: [0, 1])!
                ctx.saveGState()
                ctx.clip(to: part)
                ctx.drawLinearGradient(shade, start: CGPoint(x: part.midX, y: part.maxY), end: CGPoint(x: part.midX, y: part.minY), options: [])
                ctx.restoreGState()
                y += part.height
            }
            ctx.restoreGState()
            ctx.addPath(path)
            ctx.setStrokeColor(selected ? color(0x5AB0FF) : color(0xFFFFFF, 0.10))
            ctx.setLineWidth(max(1, (selected ? 8 : 2) * s))
            ctx.strokePath()
        }
    }
    ctx.restoreGState()
}

for (points, scale) in [(16, 1), (16, 2), (32, 1), (32, 2), (128, 1), (128, 2), (256, 1), (256, 2), (512, 1), (512, 2)] {
    let pixels = points * scale
    let ctx = CGContext(data: nil, width: pixels, height: pixels, bitsPerComponent: 8, bytesPerRow: 0, space: CGColorSpace(name: CGColorSpace.sRGB)!,
                        bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
    draw(ctx, size: CGFloat(pixels))
    let rep = NSBitmapImageRep(cgImage: ctx.makeImage()!)
    let name = "icon_\(points)x\(points)" + (scale == 2 ? "@2x" : "") + ".png"
    try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: out + "/" + name))
}
