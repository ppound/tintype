# Command line

`tintype-cli` is installed with the app and does everything the app does, on the same
library, so you can script Tintype or use it on a computer without a screen.

```sh
tintype-cli scan ~/Pictures                  # add a folder; the first scan reads everything
tintype-cli scan ~/Pictures --types jpeg     # only JPEGs from this folder (also: raw, heif, other)
tintype-cli rescan                           # later scans only read new and changed files
tintype-cli search beach sunset              # one line per stack; --unstacked lists every photo
tintype-cli search "make:fujifilm iso:>1600" --sort newest
tintype-cli search --smart dog on a beach year:2024
tintype-cli search --sort best               # your taste, once you've rated ~30 photos
tintype-cli show 678                         # everything about photo 678
tintype-cli tag 678 -a family -a beach
tintype-cli rate 678 5
tintype-cli facets camera                    # photo counts per make
tintype-cli facets camera -p FUJIFILM        # and per model
tintype-cli facets subjects -p Animals       # how many dogs, cats, birds…
tintype-cli analyze                          # look at photos for smart search, and tag subjects
tintype-cli faces                            # find faces in photos not checked yet
tintype-cli people                           # named people and unnamed groups
tintype-cli name "Ada" 113 681 871           # name faces (ids from `people`)
tintype-cli settings --sidecars off          # stop writing .xmp sidecars
tintype-cli settings --aesthetic on          # turn on the aesthetic score, then run `analyze`
```

The [search terms](searching.md#search-terms) are the same as in the app. Facet fields
are `camera`, `lenses`, `date`, `ext`, `tags`, `subjects`, `people` and `folder`.

## Options

- `--library DIR` (or the `TINTYPE_LIBRARY` environment variable) uses another library
  folder. The app takes `--library` too.
- `--json` makes any command print JSON.
- `tintype-cli exec` reads JSON commands from standard input, one per line, for
  example `{"command":"search","query":"iso:>800","limit":5}`.

Run `tintype-cli help` or `tintype-cli <command> --help` for every option.
