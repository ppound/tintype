# Tintype

Index, search and tag your photo library. One binary, no Java, Docker, WSL2 or
Python. A Rust rewrite of [PhotoStat](https://github.com/ppound/photostat).

Status: indexing, search, the desktop app, face recognition and smart
(semantic) search work. Auto-tags and near-duplicates are next (see [Roadmap](#roadmap)).

## Desktop app

```sh
cargo run --release -p tintype
```

Click **Add folder…** to add your photos. Tintype reads them where they are and
never changes, moves or deletes a photo. You get:

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

- **Smart** search, a toggle beside the search bar: describe what's in the
  picture ("dog on a beach", "red car at night") and the best matches come
  first. Field terms like `year:2024` or `rating:>=4` still filter. Tintype
  looks at each photo once first (**Analyze photos**, about 0.15 s per photo).

Face recognition and smart search run entirely on your computer; nothing is uploaded.

### Sidecars: your metadata travels with your photos

Ratings, tags and people are also saved in an XMP sidecar next to each photo
(`DSCF1234.JPG.xmp`, the same format PhotoStat writes), so they survive moving
photos around, a lost catalog, or another app. Tintype only touches the
properties it owns (`xmp:Rating`, `dc:subject`, `Iptc4xmpExt:PersonInImage`)
and keeps everything else in an existing sidecar as it was.

- Scans, and starting the app, read sidecars that changed outside Tintype; the sidecar wins. Existing
  PhotoStat sidecars are imported this way, people included (as names in the
  photo: sidecars don't say which face is whom).
- Edits in Tintype are written back right away. Sidecars are only created for
  photos that have a rating, tags or people.
- **Settings › Save ratings, tags and people in .xmp sidecars** (or
  `tintype-cli settings --sidecars off`) stops writing; sidecars are still read.
  Turning it back on writes everything edited in the meantime.

Moving or renaming photos inside your folders keeps everything Tintype knows
about them: a rescan recognizes a file by its content. The first
run downloads two small open models (YuNet, MIT; SFace, Apache-2.0; 39 MB in
total), verified by SHA-256. Smart search downloads Google's SigLIP B/16
(Apache-2.0, 815 MB) the first time.

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
tintype-cli analyze                         # embed photos for smart search
tintype-cli settings --sidecars off         # stop writing .xmp sidecars
tintype-cli search --smart dog on a beach year:2024
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

With `--smart` (the app's **Smart** toggle), terms with a `field:` still filter as
above and the remaining words are matched against what the photos show. Results are
ranked by match; photos far behind the best match are left out.

Facet fields for `tintype-cli facets`: `camera`, `lenses`, `date`, `ext`, `tags`, `people`, `folder`.

## Architecture

A Cargo workspace of small crates with enforced layering: a crate may only
depend on crates in lower layers (`cargo xtask layers`).

```
layer 0  codecs  infer  testkit
layer 1  meta
layer 2  catalog
layer 3  index  analyze  faces
layer 4  jobs
layer 5  engine
layer 6  ui-egui
layer 7  apps/tintype (desktop)  apps/tintype-cli
```

| Crate      | Role |
|------------|------|
| `meta`     | EXIF extraction in pure Rust (JPEG, TIFF-based RAW, Fujifilm RAF, HEIF, PNG, WebP); XMP sidecar reading and minimal-change writing |
| `catalog`  | SQLite: the source of truth for photos, extracted metadata, ratings and tags |
| `index`    | tantivy full-text, numeric and facet search. Derived from the catalog and rebuilt automatically when missing, outdated or out of sync |
| `jobs`     | Incremental parallel scanning with move detection; sidecar sync; batched face and analysis passes; people matching. All report progress and can be cancelled |
| `engine`   | Command registry. CLI, GUI and automation all run the same serde `Command`s |
| `codecs`   | Thumbnails and previews: DCT-scaled JPEG decoding, RAF embedded JPEGs, a disk thumbnail cache. HEIC and other RAW formats are *planned* |
| `infer`    | Model registry, verified downloads, and ONNX inference on `tract` (pure Rust, no native libraries). An ONNX Runtime backend for large models and GPUs is *planned* |
| `analyze`  | SigLIP image and text embeddings for smart search. Near-duplicates, auto-tags and an aesthetic score are *planned* |
| `faces`    | YuNet detection, landmark alignment, SFace embeddings, Chinese Whispers grouping, k-nearest matching |
| `ui-egui`  | The desktop UI. The engine runs on its own thread and thumbnails decode on a worker pool, newest request first |

Design rules:
- **Photos are read-only.** Tintype writes only to its library folder and, when
  enabled, to `.xmp` sidecars beside photos. It never deletes a folder it didn't create.
- **The catalog is the truth; the index is a cache.** Deleting `index/` loses nothing.
- **Incremental by default.** A file is re-read only when its size or mtime
  changes, or when `meta::EXTRACTOR_VERSION` is bumped. A metadata-only refresh
  keeps the existing content hash.
- **Never treat unreadable as deleted.** A scan only removes entries whose file
  is actually gone, and a cancelled scan removes nothing. A gone file whose content
  (hash, or name + size + mtime without hashing) shows up elsewhere was moved, not deleted.
- **No bundled model weights.** Models download on first use and are verified by SHA-256.
- **Faces are compared with faces, never with an average face.** Face embeddings
  share a common direction, so the mean of a large mixed set looks like everyone.
  Grouping links individually similar pairs; suggestions use a person's 3 best
  matching confirmed faces. Thresholds were tuned on a real library: a wrong
  merge is worse than a split, since naming two groups the same merges them.
- **Long jobs run in batches** (face passes: 150 photos, analysis: 200), so the app stays
  responsive and other commands run in between.
- **Models run as exported, in fp32.** On tract, int8-quantized SigLIP was ~60x slower and
  fp16 ~2x slower even translated to f32, so the smaller downloads aren't worth it there.

## Development

```sh
cargo xtask ci        # fmt check, clippy -D warnings, tests, layer check
```

Tests use synthetic photos from `testkit`; no photo corpus is needed.

`TINTYPE_SCREENSHOT=out.png tintype` runs the app until thumbnails settle, saves
a screenshot and exits. `TINTYPE_SCREENSHOT_SELECT=<n>` selects a result first,
`TINTYPE_SCREENSHOT_VIEW=loupe` opens it full size, and
`TINTYPE_SCREENSHOT_SMART="dog on a beach"` runs a smart search.

## Roadmap

1. ~~Indexer + CLI~~
2. ~~egui shell: thumbnail grid, single-photo view, facet sidebar, search~~
   Next: RAF sensor dimensions, HEIC and other RAW previews, app icon, paging for very large result sets
3. ~~Faces: detect, group, name, suggest, merge~~
   Next: faster face passes, HEIC/DNG previews for detection, scaling grouping past ~20k faces
4. ~~Smart search (SigLIP embeddings)~~
   Next: near-duplicates (perceptual hash), zero-shot auto-tags, aesthetic score
5. Packaging: MSI, DMG, AppImage from CI
6. ~~Read and write PhotoStat XMP sidecars~~
   Next: import PhotoStat's JSON data, if any; use `photostat:place`
