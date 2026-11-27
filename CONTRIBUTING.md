# Contributing to Audivane DSP

Thanks for helping. Bug reports, hardware reports, documentation fixes and
pull requests are all welcome.

## Reporting a problem

Open an [issue](https://github.com/audivane/dsp/issues) with:

- your distribution, PipeWire version (`pipewire --version`) and DAC;
- what you did, what you expected and what happened;
- the output of `avdsp ctl status`;
- the engine log around the problem: `journalctl --user -u audivane-dsp --since "10 min ago"`.

Security problems: see [SECURITY.md](SECURITY.md) instead of opening an issue.

## Pull requests

1. For anything bigger than a small fix, open an issue first so we can agree
   on the approach.
2. Keep each pull request focused on one change, and update the docs it
   affects (`docs/control-plane.md` for protocol changes, plus `npm run
   gen:types` in `app/`).
3. Run the checks before you push:

   ```sh
   cargo fmt --check
   cargo clippy --all-targets
   cargo test                 # or cargo nextest run
   (cd app && npm ci && npm run typecheck && npm test)
   ```

   [DEV.md](DEV.md) covers building, the headless test rig and running the
   engine against a loopback device.

## Contributor License Agreement

Before your first pull request can be merged, you need to sign the
[Audivane Contributor License Agreement](CLA.md). The CLA bot comments on your
pull request; reply with the sentence it shows. You sign once per repository.

You keep the copyright in your work. The CLA lets the maintainer ship your
contribution under the project's AGPL-3.0-only license and, if needed, under
other licenses as well, so that Audivane can be dual-licensed. Every public
version that includes your work stays available under an open-source license.
