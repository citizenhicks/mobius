import Foundation

#if DEBUG
let desktopCursorAsset = URL(fileURLWithPath: #filePath)
    .deletingLastPathComponent().appendingPathComponent("resources/hi.mousePointer01.svg").path
#else
let desktopCursorAsset = Bundle.main.path(forResource: "hi.mousePointer01", ofType: "svg") ?? ""
#endif
