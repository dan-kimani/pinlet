# Flatpak packaging

Build requirements:

- `flatpak` and `flatpak-builder`
- GNOME SDK 48 runtime: `flatpak install org.gnome.Sdk//48 org.gnome.Platform//48`
- rust-stable extension: `flatpak install org.freedesktop.Sdk.Extension.rust-stable//24.08`
- [flatpak-cargo-generator](https://github.com/flatpak/flatpak-builder-tools)
  to pin crate dependencies

Steps (run in this directory):

1. Generate the vendored sources:

   ```
   flatpak-cargo-generator ../Cargo.lock -o cargo-sources.json
   ```

2. Build and install:

   ```
   flatpak-builder --user --install build-dir org.pinlet.Pinlet.json --force-clean
   ```

3. Run:

   ```
   flatpak run org.pinlet.Pinlet
   ```

Notes:

- `project_license` in `assets/org.pinlet.Pinlet.metainfo.xml` currently
  says GPL-3.0-or-later as a placeholder — change it to whatever
  license the project actually adopts.
- The tray (StatusNotifier) and the global shortcut portal are
  granted explicit D-Bus access via `finish-args`.
- Git access for syncing happens outside the sandbox: the app works
  on `~/.local/share/pinlet`, which the user manages with their own
  git tooling.
