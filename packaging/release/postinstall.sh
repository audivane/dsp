#!/bin/sh
# Shared post-install note for the .deb, .rpm and Arch packages built by nfpm.
# It never starts anything: the engine is a per-user service.
cat <<'EOF'
==> Audivane DSP installed.
Start the engine for your user session (the package never does this for you):
    systemctl --user daemon-reload && systemctl --user enable --now audivane-dsp
While it runs it takes your output DAC exclusively and registers itself as the
PipeWire sink; stop the service to hand the device back. Check it with
    avdsp ctl status
and open "Audivane DSP" from your app menu.
EOF
exit 0
