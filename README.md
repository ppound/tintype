# Tintype

Index, search and tag your photo library. One binary, no Java, Docker, WSL2 or
Python. A Rust rewrite of [PhotoStat](https://github.com/ppound/photostat).

Status: the indexer and CLI work. The egui desktop app, image analysis and face
recognition are next (see [Roadmap](#roadmap)).

## Quick start

```sh
cargo build --release
tintype scan ~/Pictures                     # first scan reads everything
tintype scan ~/Pictures                     # later scans only read new/changed files
tintype search beach sunset
tintype search "make:fujifilm iso:>1600" --sort newest
tintype facets camera                       # counts per make
tintype facets camera -p FUJIFILM           # drill down to models
tintype show 678
tintype tag 678 -a family -a beach
tintype rate 678 5
```

The library (catalog + index) lives in the OS data folder; use `--library DIR`
or `TINTYPE_LIBRARY` to choose another. Add `--json` to any command for
machine-readable output. `tintype exec` reads JSON commands from stdin, one
per line, e.g. `{"command":"search","query":"iso:>800","limit":5}`.

## Search syntax

All terms must match.

| Query                          | Finds                                       |
|--------------------------------|---------------------------------------------|
| `beach sunset`                 | words anywhere: path, EXIF values, tags     |
| `make:canon` `model:x-t5` `lens:23mm` `filename:dscf` | one field |
| `iso:>1600` `aperture:<=2.8` `focal:[70 TO 200]` `shutter:<0.002` | numeric ranges |
| `year:2024` `year:[2019 TO 2021]` | capture year                             |
| `rating:>=4` `tag:family`      | your own metadata                           |
| `gps:true`                     | photos with a location                      |
| `camera:"/FUJIFILM/X-T5"` `ext:/raf` `folder:"/C:/Users/me/Pictures/2024"` | facets |

Facet fields for `tintype facets`: `camera`, `lenses`, `date`, `ext`, `tags`, `folder`.

## Architecture

A Cargo workspace of small crates with enforced layering: a crate may only
depend on crates in lower layers (`cargo xtask layers`).

```
layer 0  meta  codecs*  infer*  testkit
layer 1  catalog
layer 2  index  analyze*  faces*
layer 3  jobs
layer 4  engine
layer 5  ui-egui*
layer 6  apps/tintype-cli  apps/tintype*          (* = placeholder)
```

| Crate      | Role |
|------------|------|
| `meta`     | EXIF extraction in pure Rust (JPEG, TIFF-based RAW, Fujifilm RAF, HEIF, PNG, WebP) |
| `catalog`  | SQLite: the source of truth for photos, extracted metadata, ratings and tags |
| `index`    | tantivy full-text, numeric and facet search. Derived from the catalog and rebuilt automatically when missing, outdated or out of sync |
| `jobs`     | Incremental parallel scanning with progress reports and cancellation |
| `engine`   | Command registry. CLI, GUI and automation all run the same serde `Command`s |
| `codecs`   | *planned*: thumbnails, RAW previews, HEIC |
| `infer`    | *planned*: ONNX Runtime (`ort`) inference, model download and verification |
| `analyze`  | *planned*: perceptual hashes, CLIP embeddings, semantic search, auto-tags, aesthetic score |
| `faces`    | *planned*: YuNet detection, SFace embeddings, clustering, naming |
| `ui-egui`  | *planned*: the desktop app |

Design rules:
- **The catalog is the truth; the index is a cache.** Deleting `index/` loses nothing.
- **Incremental by default.** A file is re-read only when its size or mtime
  changes, or when `meta::EXTRACTOR_VERSION` is bumped. A metadata-only refresh
  keeps the existing content hash.
- **Never treat unreadable as deleted.** A scan only removes entries whose file
  is actually gone, and a cancelled scan removes nothing.
- **No bundled model weights.** Models download on first use and are verified by SHA-256.

## Development

```sh
cargo xtask ci        # fmt check, clippy -D warnings, tests, layer check
```

Tests use synthetic photos from `testkit`; no photo corpus is needed.

## Roadmap

1. ~~Indexer + CLI~~
2. egui shell: thumbnail grid, single-photo view, facet sidebar, search
3. Faces: detect, cluster, name, merge
4. CLIP: semantic search, auto-tags, aesthetic score, near-duplicates
5. Packaging: MSI, DMG, AppImage from CI
6. Import PhotoStat sidecars (JSON/XMP) so existing names and tags carry over
