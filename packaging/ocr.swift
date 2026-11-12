import Foundation
import Vision

let request = VNRecognizeTextRequest()
request.recognitionLevel = .accurate
request.usesLanguageCorrection = false
request.recognitionLanguages = ["en-US"]
let handler = VNImageRequestHandler(url: URL(fileURLWithPath: CommandLine.arguments[1]), options: [:])
try handler.perform([request])
// Keep alternate readings of small native glyphs (for example 7 versus / on
// Intel Tahoe). The caller still matches its exact random nonce and API port.
let text = (request.results ?? []).flatMap { $0.topCandidates(5).map { $0.string } }
let data = try JSONSerialization.data(withJSONObject: text)
print(String(data: data, encoding: .utf8)!)
