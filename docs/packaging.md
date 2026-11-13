# Packaging and releases

`scripts/package.sh` builds everything into `dist/`:

| File | For |
|---|---|
| `membli_<ver>_amd64.deb` | Ubuntu / Debian. Installs `/usr/bin/membli`, the app-menu entry, icons (32–512 px), and a systemd **user** unit in `/usr/lib/systemd/user/`. `postinst` runs `systemctl --global enable membli.service`, so the background service starts for every user at their next login; `prerm` disables it on removal. |
| `membli-<ver>-linux-x86_64.tar.gz` | Any distro without root: the binary, README, LICENSE and `install.sh` (copies to `~/.local/bin` and runs `membli install`). |
| `Membli-<ver>-x86_64.AppImage` | Built only when `appimagetool` is on your `PATH`. When run from an AppImage, `membli install` points the service and menu entry at the AppImage file itself (`$APPIMAGE`), not the temporary mount. |
| `SHA256SUMS` | Checksums for the files above. |

```sh
scripts/package.sh
```

Requirements: `cargo`, `dpkg-deb`, and optionally `appimagetool`.

## Compatibility

The binary links only against glibc (2.35, the Ubuntu 22.04 version) and loads OpenGL/EGL, X11, xkbcommon and Wayland libraries at runtime. Build releases on the oldest Ubuntu you want to support; 22.04 binaries run on 22.04 and newer.

## Icons

The logo is drawn by `scripts/make_icon.py` (Pillow), which writes `assets/icon/membli-<size>.png` and `assets/icon/membli-256.rgba`, the raw pixels used as the window icon. Run `python3 -I scripts/make_icon.py` after changing it.

## Making a release

1. Bump `version` in `Cargo.toml` and add a section to `CHANGELOG.md`.
2. `scripts/package.sh`
3. Commit, tag `v<ver>`, push, then:
   ```sh
   gh release create v<ver> dist/* --title "Membli <ver>" --notes-file <notes>
   ```

Running copies of Membli check `https://api.github.com/repos/Btocode/membli/releases/latest` when the window opens (if *Check for updates* is on) and offer the new version's release page.
