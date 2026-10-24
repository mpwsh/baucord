// Draws a picture onto an opaque square of one colour and writes it as a
// PNG without an alpha channel, which is what App Store Connect wants of
// an app icon (IOS-SPEC.md D67, MOB-3). render_icon.sh runs it:
//
//   swift flatten_icon.swift SRC.png OUT.png 1024 1A1D21
//
// A square picture fills the square; any other shape is fitted inside it
// and centred, as render_icon.sh's sips did before.
import CoreGraphics
import Foundation
import ImageIO
import UniformTypeIdentifiers

func fail(_ message: String) -> Never {
    FileHandle.standardError.write(Data("error: \(message)\n".utf8))
    exit(1)
}

let args = CommandLine.arguments
guard args.count == 5, let size = Int(args[3]), size > 0,
      args[4].count == 6, let rgb = UInt32(args[4], radix: 16)
else {
    fail("usage: flatten_icon.swift SRC OUT SIZE RRGGBB")
}
let src = URL(fileURLWithPath: args[1])
let out = URL(fileURLWithPath: args[2])

guard let source = CGImageSourceCreateWithURL(src as CFURL, nil),
      let image = CGImageSourceCreateImageAtIndex(source, 0, nil)
else {
    fail("cannot read \(src.path)")
}

// sRGB with no alpha: the fourth byte of each pixel is padding, so the
// image made from it, and the PNG written from that, have none.
guard let space = CGColorSpace(name: CGColorSpace.sRGB),
      let context = CGContext(
          data: nil, width: size, height: size, bitsPerComponent: 8,
          bytesPerRow: 0, space: space,
          bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue)
else {
    fail("cannot make a \(size)x\(size) RGB canvas")
}

let channel = { (shift: UInt32) -> CGFloat in CGFloat((rgb >> shift) & 0xFF) / 255 }
let canvas = CGRect(x: 0, y: 0, width: size, height: size)
context.setFillColor(red: channel(16), green: channel(8), blue: channel(0), alpha: 1)
context.fill(canvas)

let width = CGFloat(image.width)
let height = CGFloat(image.height)
let scale = min(canvas.width / width, canvas.height / height)
let drawn = CGRect(
    x: (canvas.width - width * scale) / 2, y: (canvas.height - height * scale) / 2,
    width: width * scale, height: height * scale)
context.interpolationQuality = .high
context.draw(image, in: drawn)

guard let flat = context.makeImage(),
      let destination = CGImageDestinationCreateWithURL(
          out as CFURL, UTType.png.identifier as CFString, 1, nil)
else {
    fail("cannot write \(out.path)")
}
CGImageDestinationAddImage(destination, flat, nil)
if !CGImageDestinationFinalize(destination) {
    fail("cannot write \(out.path)")
}
