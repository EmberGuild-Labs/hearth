# macOS integration

```sh
./macos/build-app.sh              # install to /Applications
./macos/build-app.sh ~/Applications
./macos/build-app.sh --uninstall
```

Four files:

| | |
|---|---|
| `build-app.sh` | builds and installs `Hearth.app` |
| `app/main.swift` | the application: a window that views and edits a file |
| `quicklook/PreviewProvider.swift` | the Quick Look preview extension |
| `quicklook/HearthQuickLook.entitlements` | the sandbox entitlement it needs to load |

Nothing here needs Xcode. `swiftc`, `iconutil`, `codesign` and `lsregister`
all ship with the Command Line Tools, which is the difference between a build
step that runs in CI and one that needs an IDE open.

## What the app is for

macOS will not attach an icon to a bare file extension. The icon has to be
exported by an *installed application* that declares the type through a
Uniform Type Identifier. That is the only reason `Hearth.app` exists. Once it
does, two more things follow nearly free:

* **Quick Look** — pressing space renders the file's `SUMM` chunk. The
  extension shells out to the `hearth` binary carried inside the bundle, so
  there is one implementation of what an Ember file looks like and the pane
  and the terminal cannot drift apart. `hearth preview <file>` prints exactly
  what the pane shows.
* **A window** — double-clicking a file, or `hearth open <file>`, shows the
  payload as editable text with the container's facts above it. Save runs
  `hearth edit --from` on the file, so a save from the window is the same
  operation as a save from a terminal: same round trip, same refusal on a
  sealed file, same provenance entry. Images open read-only.
* **⌘N** — a save panel with a format popup, and one field for the thing that
  format cannot invent: a title for `.emt` and `.emd`, columns for `.emx`, a
  size for `.emi`, nothing for `.emc`. It runs `hearth create`, so a file made
  here and a file made from a terminal are the same file.

## The parts that are not obvious

Each of these was learned by watching it fail silently, which is the failure
mode this whole area specialises in.

**Re-sign after writing the plist.** Every `PlistBuddy` write breaks the
bundle's signature, and macOS ignores type declarations coming from a bundle
whose signature does not verify — the app registers as a handler, and the
files keep a generic icon. So signing is the last step, not the first.

**Do not sign the app with `--deep`.** A deep signature re-signs the nested
`.appex`, stripping the entitlements it was just given. The bundle verifies,
the extension is registered, and previews never appear.

**The extension must carry `com.apple.security.app-sandbox`.** The Quick Look
extension point declares its own sandbox profile (`quicklook-preview`) and
will not load an extension that is not sandboxed to begin with.

**The extension's `Info.plist` needs the keys Xcode writes** —
`CFBundleSupportedPlatforms` and the `DT*` build-provenance keys — not just
the documented `NSExtension` dictionary.

**Type declarations are "untrusted" until the app has run once**, and an
untrusted declaration's icon is ignored. The script launches the app, quits
it, and then greps `lsregister -dump` to check. It tests for `untrusted`
rather than `trusted`, because the second is a substring of the first and
would match either way.

**Finder caches icons harder than anything else here.** The script pokes
Launch Services, Icon Services, the Dock and Finder. If an icon still has not
appeared, logging out is the only thing that reliably forces it.

## Checking it worked

```sh
mdls -name kMDItemContentType notes.emt      # xyz.ember.emt
pluginkit -m -p com.apple.quicklook.preview  # + xyz.ember.hearth.quicklook
```

The first says the file type is registered; the second says the preview
extension is installed and enabled. `qlmanage -p` is *not* a reliable check on
recent macOS — it fails against Apple's own data-based preview extensions
too. Pressing space in Finder is the real test.

If a preview pane appears but says "could not run hearth", the extension
loaded and its sandbox refused to launch the binary; that is the one part of
this that a future macOS could tighten, and the answer would be to link the
preview code in rather than spawn it.
