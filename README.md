# Tintype

Index, search and tag your photo library. One binary, no Java, Docker, WSL2 or
Python. A Rust rewrite of [PhotoStat](https://github.com/ppound/photostat).

Status: indexing, search, the desktop app and face recognition work. Image
analysis (semantic search, auto-tags, aesthetic score) is next (see [Roadmap](#roadmap)).

## Desktop app

```sh
cargo run --release -p tintype
```

Click **Add folder…** to add your photos. Tintype reads them where they are;
nothing is moved or changed. You get:

- a search bar (same syntax as below), with facet filters for camera, lens,
  date, tags, file type and folder in the sidebar
- a thumbnail grid. Size it with the slider; arrow keys move the selection.
- a full-size view: double-click or Enter to open, ← → to browse, Esc to go back
- details, a 1–5 star rating (or keys 0–5) and tags for the selected photo
- **Rescan** to pick up new, changed and removed files in all your folders
- a **People** tab: Tintype finds faces, groups the ones that look alike, and
  you name a group once. New faces that match a named person are suggested for
  you to confirm. Faces show as boxes in the full-size view (F toggles them)
  and in the details panel, and `person:` / the People facet find their photos.

Face recognition runs entirely on your computer; nothing is uploaded. The first
run downloads two small open models (YuNet, MIT; SFace, Apache-2.0; 39 MB in
total), verified by SHA-256.

## Command line

```sh
tintype-cli scan ~/Pictures                 # first scan reads everything
tintype-cli rescan                          # later scans only read new/changed files
tintype-cli search beach sunset
tintype-cli search "make:fujifilm iso:>1600" --sort newest
tintype-cli facets camera                   # counts per make
tintype-cli facets camera -p FUJIFILM       # drill down to models
tintype-cli show 678
tintype-cli tag 678 -a family -a beach
tintype-cli rate 678 5
tintype-cli faces                           # find faces in photos not yet checked
tintype-cli people                          # named people and unnamed groups
tintype-cli name "Ada" 113 681 871          # name faces (ids from `people`)
```

The library (catalog, index and thumbnail cache) lives in the OS data folder,
shared by the app and the CLI. Use `--library DIR` or `TINTYPE_LIBRARY` to
choose another. Add `--json` to any CLI command for machine-readable output.
`tintype-cli exec` reads JSON commands from stdin, one per line, e.g.
`{"command":"search","query":"iso:>800","limit":5}`.

## Search syntax

All terms must match.

| Query                          | Finds                                       |
|--------------------------------|---------------------------------------------|
| `beach sunset`                 | words anywhere: path, EXIF values, tags     |
| `make:canon` `model:x-t5` `lens:23mm` `filename:dscf` | one field |
| `iso:>1600` `aperture:<=2.8` `focal:[70 TO 200]` `shutter:<0.002` | numeric ranges |
| `year:2024` `year:[2019 TO 2021]` | capture year                             |
| `rating:>=4` `tag:family`      | your own metadata                           |
| `person:ada` `faces:>=3`       | people (named faces) and number of faces    |
| `gps:true`                     | photos with a location                      |
| `camera:"/FUJIFILM/X-T5"` `ext:/raf` `folder:"/C:/Users/me/Pictures/2024"` | facets |

Facet fields for `tintype-cli facets`: `camera`, `lenses`, `date`, `ext`, `tags`, `people`, `folder`.

## Architecture

A Cargo workspace of small crates with enforced layering: a crate may only
depend on crates in lower layers (`cargo xtask layers`).

```
layer 0  codecs  infer  testkit
layer 1  meta
layer 2  catalog
layer 3  index  analyze*  faces
layer 4  jobs
layer 5  engine
layer 6  ui-egui
layer 7  apps/tintype (desktop)  apps/tintype-cli          (* = placeholder)
```

| Crate      | Role |
|------------|------|
| `meta`     | EXIF extraction in pure Rust (JPEG, TIFF-based RAW, Fujifilm RAF, HEIF, PNG, WebP) |
| `catalog`  | SQLite: the source of truth for photos, extracted metadata, ratings and tags |
| `index`    | tantivy full-text, numeric and facet search. Derived from the catalog and rebuilt automatically when missing, outdated or out of sync |
| `jobs`     | Incremental parallel scanning; batched face passes and people matching. All report progress and can be cancelled |
| `engine`   | Command registry. CLI, GUI and automation all run the same serde `Command`s |
| `codecs`   | Thumbnails and previews: DCT-scaled JPEG decoding, RAF embedded JPEGs, a disk thumbnail cache. HEIC and other RAW formats are *planned* |
| `infer`    | Model registry, verified downloads, and ONNX inference on `tract` (pure Rust, no native libraries). An ONNX Runtime backend for large models and GPUs is *planned* |
| `analyze`  | *planned*: perceptual hashes, CLIP embeddings, semantic search, auto-tags, aesthetic score |
| `faces`    | YuNet detection, landmark alignment, SFace embeddings, Chinese Whispers grouping, k-nearest matching |
| `ui-egui`  | The desktop UI. The engine runs on its own thread and thumbnails decode on a worker pool, newest request first |

Design rules:
- **The catalog is the truth; the index is a cache.** Deleting `index/` loses nothing.
- **Incremental by default.** A file is re-read only when its size or mtime
  changes, or when `meta::EXTRACTOR_VERSION` is bumped. A metadata-only refresh
  keeps the existing content hash.
- **Never treat unreadable as deleted.** A scan only removes entries whose file
  is actually gone, and a cancelled scan removes nothing.
- **No bundled model weights.** Models download on first use and are verified by SHA-256.
- **Faces are compared with faces, never with an average face.** Face embeddings
  share a common direction, so the mean of a large mixed set looks like everyone.
  Grouping links individually similar pairs; suggestions use a person's 3 best
  matching confirmed faces. Thresholds were tuned on a real library: a wrong
  merge is worse than a split, since naming two groups the same merges them.
- **Long jobs run in batches** (face passes: 150 photos), so the app stays responsive and other
  commands run in between.

## Development

```sh
cargo xtask ci        # fmt check, clippy -D warnings, tests, layer check
```

Tests use synthetic photos from `testkit`; no photo corpus is needed.

`TINTYPE_SCREENSHOT=out.png tintype` runs the app until thumbnails settle, saves
a screenshot and exits. `TINTYPE_SCREENSHOT_SELECT=<n>` selects a result first,
and `TINTYPE_SCREENSHOT_VIEW=loupe` opens it full size.

## Roadmap

1. ~~Indexer + CLI~~
2. ~~egui shell: thumbnail grid, single-photo view, facet sidebar, search~~
   Next: RAF sensor dimensions, HEIC and other RAW previews, app icon, paging for very large result sets
3. ~~Faces: detect, group, name, suggest, merge~~
   Next: faster face passes, HEIC/DNG previews for detection, scaling grouping past ~20k faces
4. CLIP: semantic search, auto-tags, aesthetic score, near-duplicates
5. Packaging: MSI, DMG, AppImage from CI
6. Import PhotoStat sidecars (JSON/XMP) so existing names and tags carry over
