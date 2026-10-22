// Lists the windows of a process: id, layer, width, height, on-screen flag and title. Used by shoot.sh.
import CoreGraphics
let list = CGWindowListCopyWindowInfo([.optionAll], kCGNullWindowID) as! [[String: Any]]
for w in list where (w[kCGWindowOwnerPID as String] as? Int) == Int(CommandLine.arguments[1])! {
    let b = w[kCGWindowBounds as String] as! [String: Any]
    print(w[kCGWindowNumber as String]!, "layer", w[kCGWindowLayer as String]!, "w", b["Width"]!, "h", b["Height"]!, "onscreen", w[kCGWindowIsOnscreen as String] ?? "-", w[kCGWindowName as String] ?? "")
}
