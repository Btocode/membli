#!/usr/bin/env bash
# Build Membli's release packages into dist/:
#   membli_<ver>_amd64.deb        Ubuntu / Debian package
#   Membli-<ver>-x86_64.AppImage  single file, runs on most distros
#   membli-<ver>-linux-x86_64.tar.gz  binary + install.sh
# Usage: scripts/package.sh   (needs cargo and dpkg-deb; the AppImage also needs appimagetool on PATH)
set -euo pipefail
cd "$(dirname "$0")/.."
VER=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
DIST=dist
rm -rf "$DIST" && mkdir -p "$DIST"

echo "==> building membli $VER"
cargo build --release
BIN=target/release/membli

desktop() { # $1 = Exec
  cat <<EOF
[Desktop Entry]
Type=Application
Name=Membli
GenericName=System Monitor
Comment=Sci-fi control center for this PC
Exec=$1
Icon=membli
Categories=System;Monitor;
Keywords=monitor;performance;cleanup;disk;cpu;memory;
Terminal=false
StartupWMClass=membli
EOF
}

# ---------- .deb ----------
echo "==> .deb"
D=$(mktemp -d)
install -Dm755 "$BIN" "$D/usr/bin/membli"
desktop membli > "$D/membli.desktop" && install -Dm644 "$D/membli.desktop" "$D/usr/share/applications/membli.desktop" && rm "$D/membli.desktop"
for s in 32 48 64 128 256 512; do
  install -Dm644 "assets/icon/membli-$s.png" "$D/usr/share/icons/hicolor/${s}x${s}/apps/membli.png"
done
install -Dm644 /dev/stdin "$D/usr/lib/systemd/user/membli.service" <<'EOF'
[Unit]
Description=Membli background service

[Service]
ExecStart=/usr/bin/membli daemon
Restart=on-failure
Nice=10
IOSchedulingClass=idle

[Install]
WantedBy=default.target
EOF
install -Dm644 LICENSE "$D/usr/share/doc/membli/copyright"
install -Dm644 assets/fonts/OFL.txt "$D/usr/share/doc/membli/OFL-fonts.txt"
mkdir -p "$D/DEBIAN"
SIZE=$(du -sk "$D/usr" | cut -f1)
cat > "$D/DEBIAN/control" <<EOF
Package: membli
Version: $VER
Section: utils
Priority: optional
Architecture: amd64
Installed-Size: $SIZE
Depends: libc6 (>= 2.35), libgl1, libegl1, libx11-6, libxkbcommon0, libxkbcommon-x11-0
Recommends: libnotify-bin, pkexec | policykit-1, power-profiles-daemon
Suggests: zram-tools
Maintainer: Membli <https://github.com/Btocode/membli>
Homepage: https://github.com/Btocode/membli
Description: Sci-fi control center for your Ubuntu PC
 Membli shows what makes the desktop slow (disk stalls, memory pressure,
 heat, runaway processes, log floods), records every freeze with its cause,
 guards against heat and memory freezes, cleans up safely and offers
 one-click fixes. It can ask Claude for advice on a measured report.
EOF
cat > "$D/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
# start the background service for every user at their next login
systemctl --global enable membli.service >/dev/null 2>&1 || true
command -v update-desktop-database >/dev/null && update-desktop-database -q /usr/share/applications || true
command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -q -t /usr/share/icons/hicolor || true
exit 0
EOF
cat > "$D/DEBIAN/prerm" <<'EOF'
#!/bin/sh
set -e
if [ "$1" = "remove" ] || [ "$1" = "purge" ]; then
  systemctl --global disable membli.service >/dev/null 2>&1 || true
fi
exit 0
EOF
chmod 755 "$D/DEBIAN/postinst" "$D/DEBIAN/prerm"
dpkg-deb --root-owner-group --build "$D" "$DIST/membli_${VER}_amd64.deb" >/dev/null
rm -rf "$D"

# ---------- AppImage (only when appimagetool is installed) ----------
AT=$(command -v appimagetool || command -v appimagetool-x86_64.AppImage || true)
if [ -z "$AT" ]; then
  echo "==> AppImage skipped (put appimagetool on PATH to build it)"
else
echo "==> AppImage"
A=$(mktemp -d)/Membli.AppDir
mkdir -p "$A/usr/bin"
install -m755 "$BIN" "$A/usr/bin/membli"
desktop membli > "$A/membli.desktop"
install -m644 assets/icon/membli-256.png "$A/membli.png"
install -Dm644 assets/icon/membli-256.png "$A/usr/share/icons/hicolor/256x256/apps/membli.png"
cat > "$A/AppRun" <<'EOF'
#!/bin/sh
HERE="$(dirname "$(readlink -f "$0")")"
exec "$HERE/usr/bin/membli" "$@"
EOF
chmod 755 "$A/AppRun"
ARCH=x86_64 "$AT" --no-appstream "$A" "$DIST/Membli-${VER}-x86_64.AppImage" >/dev/null 2>&1
rm -rf "$(dirname "$A")"
fi

# ---------- tarball ----------
echo "==> tar.gz"
T=$(mktemp -d)/membli-$VER
mkdir -p "$T"
install -m755 "$BIN" "$T/membli"
cp README.md LICENSE "$T/"
cat > "$T/install.sh" <<'EOF'
#!/bin/sh
# Installs Membli for this user: ~/.local/bin/membli, app-menu entry, background service.
set -e
cd "$(dirname "$0")"
install -Dm755 membli "$HOME/.local/bin/membli"
"$HOME/.local/bin/membli" install
echo "Done. Open Membli from the app menu."
EOF
chmod 755 "$T/install.sh"
tar -C "$(dirname "$T")" -czf "$DIST/membli-${VER}-linux-x86_64.tar.gz" "membli-$VER"
rm -rf "$(dirname "$T")"

(cd "$DIST" && sha256sum * > SHA256SUMS)
echo "==> done"
ls -la "$DIST"
