# Bundled font

`Roboto-Bold-subset.ttf` is a subset of **Roboto Bold** (Copyright 2015 Google
Inc.), used by the DLL to bake format labels (`CBZ`, `EPUB`, …) into archive
thumbnails when the identification-label overlay is enabled.

Roboto is licensed under the Apache License 2.0 — see `LICENSE-Roboto.txt`.

## Why a subset

Labels only ever use uppercase Latin letters and digits, so the font is cut down
to `A`–`Z` and `0`–`9`. That drops the full ~500 KB face to a few KB, which keeps
the Explorer-resident DLL small.

## Reproducing the subset

```sh
# Source: https://github.com/googlefonts/roboto-2 (src/hinted/Roboto-Bold.ttf)
pyftsubset Roboto-Bold.ttf \
  --unicodes="U+0030-0039,U+0041-005A" \
  --output-file=Roboto-Bold-subset.ttf \
  --no-hinting --desubroutinize --layout-features='' --notdef-outline \
  --drop-tables+=DSIG
```
