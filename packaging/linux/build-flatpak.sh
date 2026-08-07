#!/usr/bin/env bash
# Flatpak is intentionally not a Milestone 7 production channel.
set -euo pipefail
cat >&2 <<'EOF'
build-flatpak.sh: Flatpak distribution is disabled.
The current CPAL backend cannot consume XDG ScreenCast's granted PipeWire FD
and selected node IDs. Shipping with direct audio/device permissions would
violate koe's sandbox trust boundary. Milestone 7 publishes the signed AppImage;
implement a portal-backed AudioBackend and portal grant/denial HIL before
re-enabling this builder.
EOF
exit 2
