# Platform packages

All packages contain the same shared Rust service and embedded Tauri frontend.
`bundle.py` packages binaries already built on the target operating system; it
does not cross-compile them or download executable dependencies.

- Windows: portable ZIP, desktop and headless executables.
- macOS: native `.app` and DMG, plus a separate headless archive. Universal
  packaging combines separately built Intel and Apple Silicon binaries with
  `lipo`, checks their architectures, and signs the finished app ad hoc.
- Linux x86_64: DEB and RPM install desktop/headless binaries, a desktop entry,
  icon, licenses and build passport. Neither package starts services or modifies
  user credentials. Removing/upgrading the package preserves per-user data.

Default data locations:

| System | Directory |
| --- | --- |
| Windows | `local_auth` next to the executable |
| macOS | `~/Library/Application Support/praxis-relay` |
| Linux | `$XDG_DATA_HOME/praxis-relay`, otherwise `~/.local/share/praxis-relay` |

`RELAY_AUTH_DIR` wins on every platform. `RELAY_LOCAL=1` retains the explicit
working-directory embedding contract. A pre-existing adjacent `local_auth`
directory remains a portable installation. Unix data roots are private (0700),
and newly written account tokens/client keys/control capabilities are 0600.
Do not place a portable data directory inside a read-only application bundle.

On Unix, the optional `instructions.txt` lives in the data directory unless
`RELAY_INSTRUCTIONS_FILE` specifies another file. Windows keeps its existing
next-to-executable instruction-file behavior.

## Runtime dependencies

The desktop uses system WebKit: WKWebView on macOS and WebKitGTK 4.1 on Linux.
DEB/RPM dependency metadata installs the GTK/WebKit/AppIndicator runtime.
Python 3.10+ with SSL is needed only for browser sign-in, not serving requests.
Finder launches probe Python.org and Homebrew installation locations directly.
The headless program has no graphical runtime dependency; Linux additionally
needs the system OpenSSL runtime and trusted CA certificates.

macOS Close/Minimize hide to the menu-bar icon and remove the Dock icon;
restoring the window restores normal Dock activation. Command+Q quits and stops
the relay. Linux tray controls use their context menu. If no notification host
is available, the window minimizes normally and remains on the taskbar instead
of disappearing. Opening the desktop entry again restores the existing window.

## Signing

An ad-hoc macOS signature verifies bundle integrity but is not Apple Developer
ID signing or notarization. macOS can require approval in Privacy & Security for
an Internet download. The release must state the actual signing status; never
claim a notarized app without a verified notarization ticket.
