# Changelog

## 0.2.0 — 2026-11-13

### Added
- **Packages**: a `.deb` for Ubuntu/Debian (starts the background service for every user) and a no-root tarball with `install.sh`; `scripts/package.sh` builds them, and an AppImage too when `appimagetool` is installed.
- **Copy anything**: selecting text copies it to the clipboard as soon as you let go, with a "Copied" toast. Ctrl+C works.
- **Display modes** in Settings: Auto (smooth while the CPU has room, slower while the PC is busy), Smooth (60 fps), or Interval (every 1–60 s, no animation).
- **Heat guard**: switches to the power-saver profile while the CPU stays above 88 °C, back below 75 °C.
- **Memory warning** before RAM and swap run out, and an optional **memory guard** that closes the biggest app (earlyoom style) to stop a freeze.
- **oomd watch**: shows when Ubuntu's own memory killer ends something.
- **Desktop notifications** for long freezes and guard actions.
- **New one-click tunes**: turn on zram (compressed swap in RAM), tune memory for a slow SSD (dirty limits, cache pressure, swappiness), slow down GNOME's file indexer.
- **Browser caches can be part of the automatic cleanup**, only while the browser is closed (off by default).
- **Update check** against GitHub releases, with *Check now* and a download link; `membli --version`.
- **New logo**: an abstract orbital bloom, used for the window, app menu and packages.

### Changed
- Smoother screen: 60 fps while focused, numbers and bars ease to new values, live charts glide between samples, and the window shares history instead of copying it every frame.
- Thumbnails are cleaned by age (unused for 30 days) instead of all at once.
- `membli install` installs PNG icons in all sizes and works from an AppImage.

### Fixed
- Copying text from the window did nothing (the clipboard feature was missing).

## 0.1.0 — 2026-11-12

First release: live board, findings with one-click fixes, processes, disk and space, three-level cleanup, 30-day history with a freeze log, background service, and Claude insights.
