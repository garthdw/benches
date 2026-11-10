// Draws the 1024px app icon PNG at the path given as the first argument.
import AppKit

let background = CGColor(red: 0x14 / 255, green: 0x15 / 255, blue: 0x17 / 255, alpha: 1)
let foreground = CGColor(red: 0xd8 / 255, green: 0xd9 / 255, blue: 0xda / 255, alpha: 1)
let size = 1024

guard CommandLine.arguments.count == 2,
    let context = CGContext(
        data: nil, width: size, height: size, bitsPerComponent: 8, bytesPerRow: 0,
        space: CGColorSpace(name: CGColorSpace.sRGB)!,
        bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)
else {
    FileHandle.standardError.write("usage: icon.swift OUT.png\n".data(using: .utf8)!)
    exit(2)
}

// Apple's icon grid: an 824pt body centred on the 1024pt canvas.
let body = CGRect(x: 100, y: 100, width: 824, height: 824)
context.addPath(CGPath(roundedRect: body, cornerWidth: 185, cornerHeight: 185, transform: nil))
context.setFillColor(background)
context.fillPath()

// A strip of three tabs, the middle one selected: a project is a group of terminals.
for (index, alpha) in [0.25, 0.9, 0.25].enumerated() {
    let tab = CGRect(x: 220 + CGFloat(index) * 200, y: 740, width: 176, height: 40)
    context.addPath(CGPath(roundedRect: tab, cornerWidth: 20, cornerHeight: 20, transform: nil))
    context.setFillColor(foreground.copy(alpha: alpha)!)
    context.fillPath()
}

// A prompt chevron and an underline cursor.
context.setStrokeColor(foreground)
context.setLineWidth(56)
context.setLineCap(.round)
context.setLineJoin(.round)
context.move(to: CGPoint(x: 260, y: 600))
context.addLine(to: CGPoint(x: 420, y: 450))
context.addLine(to: CGPoint(x: 260, y: 300))
context.strokePath()
context.setFillColor(foreground)
context.fill(CGRect(x: 500, y: 272, width: 220, height: 64))

let url = URL(fileURLWithPath: CommandLine.arguments[1])
guard let image = context.makeImage(),
    let destination = CGImageDestinationCreateWithURL(url as CFURL, "public.png" as CFString, 1, nil)
else { exit(1) }
CGImageDestinationAddImage(destination, image, nil)
exit(CGImageDestinationFinalize(destination) ? 0 : 1)
