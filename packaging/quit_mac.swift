// Request normal application termination for the exact owned QA process.
// Bundle-id lookup can select another registered copy/profile of the app.
import AppKit
import Darwin

guard CommandLine.arguments.count == 2,
      let pid = Int32(CommandLine.arguments[1]),
      let application = NSRunningApplication(processIdentifier: pid) else {
    fputs("QA application process was not found\n", stderr)
    exit(2)
}
guard application.terminate() else {
    fputs("QA application refused the native Quit request\n", stderr)
    exit(3)
}
print("Requested native Quit for PID \(pid)")
