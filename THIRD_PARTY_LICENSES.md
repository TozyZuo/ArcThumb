# Third-party licenses

ArcThumb itself is distributed under **MIT OR Apache-2.0** (see
`LICENSE-MIT` and `LICENSE-APACHE`). The following third-party
components are redistributed with ArcThumb (the `arcthumb.dll` shell
extension and/or `arcthumb-config.exe`) and require separate
acknowledgement.

## LibVLC playback runtime

The installer includes the unmodified x64 runtime from VideoLAN's
[VideoLAN.LibVLC.Windows 3.0.23.1](https://www.nuget.org/packages/VideoLAN.LibVLC.Windows/3.0.23.1)
package, licensed **LGPL-2.1-or-later**. Copyright VideoLAN and VLC authors.
LibVLC and its codec modules (including FFmpeg's libavcodec) have their own
licenses; upstream notices and accompanying files are retained in `libvlc/`.
The LGPL text is also included as `libvlc/COPYING-LibVLC.txt`.

ArcThumb dynamically loads this separate runtime. It does not modify or
statically link LibVLC. Users may replace the runtime with an ABI-compatible
LibVLC 3 x64 build and may debug changes to these libraries. ArcThumb imposes
no restriction on reverse engineering for that purpose.

Upstream source and build instructions:

- [LibVLC NuGet packaging source](https://code.videolan.org/videolan/libvlc-nuget)
- [VLC 3.0.23 source archive](https://download.videolan.org/pub/videolan/vlc/3.0.23/vlc-3.0.23.tar.xz)
- [VLC source and contrib dependency build recipes](https://code.videolan.org/videolan/vlc/-/tree/3.0.23)

`scripts/prepare-libvlc.ps1` records the exact binary package version, download
location and checksum used for this build. The runtime can be built and
replaced independently of ArcThumb.

## Slint

[Slint](https://slint.dev/) is used as the GUI toolkit for
`arcthumb-config.exe` under the **Slint Royalty-Free License 2.0**.

Full license text:
https://github.com/slint-ui/slint/blob/master/LICENSES/LicenseRef-Slint-Royalty-free-2.0.md

Attribution: ArcThumb satisfies the Slint Royalty-Free License 2.0
attribution requirement by displaying the `AboutSlint` widget inside
the **About** dialog of `arcthumb-config.exe` (reachable via the
**About** button in the settings window). The badge shows the Slint
logo and links back to https://slint.dev/.

Slint's own source is not modified and is linked statically into the
binary via the `slint` crate.

## Roboto (font)

`arcthumb.dll` embeds an A–Z / 0–9 subset of **Roboto Bold**
(Copyright 2015 Google Inc.) to draw the format labels in the
identification overlay. Roboto is licensed under the **Apache License
2.0**.

The subset font and a copy of its license live in `assets/fonts/`
(`Roboto-Bold-subset.ttf`, `LICENSE-Roboto.txt`); the subsetting
command is recorded in `assets/fonts/README.md`. Only the glyph data
is reduced — the outlines themselves are unmodified.

---

Other Rust crates used by ArcThumb (both the DLL and
`arcthumb-config.exe`) are redistributed under their respective MIT,
Apache-2.0, BSD, or similarly permissive licenses. Running
`cargo tree --format '{p} {l}'` from the repository root will list
every dependency together with its SPDX license identifier.
