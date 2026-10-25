// Loads https://example.com in a WKWebView and idles: a WebKit client that must make
// `script/photon-netcheck -- swift tooling/photon-perf/webkit-proof.swift` fail.
import Cocoa
import WebKit

let app = NSApplication.shared
app.setActivationPolicy(.prohibited)
let view = WKWebView(frame: NSRect(x: 0, y: 0, width: 100, height: 100))
view.load(URLRequest(url: URL(string: "https://example.com")!))
app.run()
