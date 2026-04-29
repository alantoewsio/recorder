# LAME MP3 encoder (third party)

This directory vendors **LAME** (LAME Ain’t an MP3 Encoder) for MP3 encoding used via the Rust crate [`mp3lame-encoder`](https://crates.io/crates/mp3lame-encoder), which loads the native LAME library at runtime.

## Contents

| Path | Description |
|------|-------------|
| `LICENSE` | Short FAQ from the upstream LAME tree (commercial use, linking). |
| `COPYING` | Full **GNU Library General Public License, version 2** (LGPL 2.0 / “LibGPL 2”) as distributed with LAME. |
| `windows-x64/libmp3lame.dll` | Prebuilt **64-bit Windows** `libmp3lame` (LAME **3.100**) from [RareWares](https://www.rarewares.org/mp3-lame-libraries.php) (`libmp3lame-3.100x64.zip`). |

Upstream LAME source releases: [SourceForge — lame / 3.100](https://sourceforge.net/projects/lame/files/lame/3.100/).

## LGPL compliance (summary)

This project links to LAME **as a separate shared library** (not statically relinking LAME into the Rust binary). You should still:

1. **Ship** `libmp3lame.dll` (or your own LGPL-compliant build) alongside the application **or** ensure it is discoverable on `PATH`, together with the license texts in this directory.
2. **Acknowledge** use of LAME and point users to the LAME project (see `LICENSE` and the project `README.md`).
3. If you **modify LAME itself**, LGPL requires you to publish those changes under the same license.

This is not legal advice; consult counsel for distribution in your jurisdiction.

## MP3 patents and standards

MP3 is covered by patents in some jurisdictions. LAME’s own `LICENSE` file discusses LGPL use; patent licensing is separate from copyright. The authors of this application do not grant any patent license.

## Reproducing this folder

Run from the repository root:

```powershell
pwsh scripts/vendor_lame.ps1
```

The script re-downloads the official source tarball (for `LICENSE` / `COPYING`) and the RareWares Windows x64 ZIP (for `libmp3lame.dll`).
