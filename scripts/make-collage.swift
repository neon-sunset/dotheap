// Composes the README image from four window screenshots of the same size (scripts/shoot.sh): two rows, the second
// shifted to the right, each window with a shadow on a transparent background.
// usage: swift scripts/make-collage.swift <out.png> <top left> <top right> <bottom left> <bottom right>
import AppKit

let args = CommandLine.arguments
guard args.count == 6 else { print("usage: make-collage.swift <out.png> <4 screenshots>"); exit(1) }
let images = args[2...].map { path -> CGImage in
    guard let data = NSData(contentsOfFile: path), let source = CGImageSourceCreateWithData(data, nil),
          let image = CGImageSourceCreateImageAtIndex(source, 0, nil) else { print("cannot read \(path)"); exit(1) }
    return image
}
// Pixels per point: screencapture writes 144 dpi for a window on a Retina display.
let properties = CGImageSourceCreateWithURL(URL(fileURLWithPath: args[2]) as CFURL, nil).flatMap { CGImageSourceCopyPropertiesAtIndex($0, 0, nil) as? [CFString: Any] }
let scale = ((properties?[kCGImagePropertyDPIWidth] as? Double) ?? 72) / 72

func color(_ hex: UInt32, _ alpha: CGFloat = 1) -> CGColor {
    CGColor(srgbRed: CGFloat(hex >> 16 & 0xFF) / 255, green: CGFloat(hex >> 8 & 0xFF) / 255, blue: CGFloat(hex & 0xFF) / 255, alpha: alpha)
}

let margin = 70 * scale, gap = 50 * scale, shift = 140 * scale, overlap = 120 * scale
let w = CGFloat(images[0].width), h = CGFloat(images[0].height)
let width = margin * 2 + w * 2 + gap + shift, height = margin * 2 + h * 2 - overlap
guard let ctx = CGContext(data: nil, width: Int(width), height: Int(height), bitsPerComponent: 8, bytesPerRow: 0,
                          space: CGColorSpace(name: CGColorSpace.sRGB)!, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue) else { exit(1) }

// Core Graphics puts the origin at the bottom left. The bottom row is drawn last, over the lower part of the top row,
// so each window keeps its title and header in view.
let frames = [
    CGRect(x: margin + shift, y: margin, width: w, height: h),
    CGRect(x: margin + shift + w + gap, y: margin, width: w, height: h),
    CGRect(x: margin, y: margin + h - overlap, width: w, height: h),
    CGRect(x: margin + w + gap, y: margin + h - overlap, width: w, height: h),
]
for (image, frame) in [(images[0], frames[2]), (images[1], frames[3]), (images[2], frames[0]), (images[3], frames[1])] {
    ctx.saveGState()
    ctx.setShadow(offset: CGSize(width: 0, height: -18 * scale), blur: 50 * scale, color: color(0x000000, 0.6))
    ctx.draw(image, in: frame)
    ctx.restoreGState()
}

let out = ctx.makeImage()!
let destination = CGImageDestinationCreateWithURL(URL(fileURLWithPath: args[1]) as CFURL, "public.png" as CFString, 1, nil)!
CGImageDestinationAddImage(destination, out, nil)
guard CGImageDestinationFinalize(destination) else { print("cannot write \(args[1])"); exit(1) }
print("\(args[1]): \(Int(width)) x \(Int(height))")
