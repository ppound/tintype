//! `tintype-cli`: the command line. Every subcommand maps to one engine
//! [`Command`]; `tintype exec` reads commands as JSON lines from stdin.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use tintype_engine::{Command, Engine, Event, FileKind, PhotoRecord, PhotoRef, Response, ScanProgress, Sort};

// The system allocator serializes the many small allocations of parallel image
// decoding and inference; mimalloc lets face passes scale across cores.
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Parser)]
#[command(name = "tintype-cli", version, about = "Index, search and tag your photos")]
struct Cli {
    /// Library folder (catalog + index). Defaults to the OS data folder.
    #[arg(long, global = true, env = "TINTYPE_LIBRARY")]
    library: Option<PathBuf>,
    /// Print responses as JSON.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Add folders to the library, or refresh them (only new and changed files are read).
    Scan {
        #[arg(required = true)]
        folders: Vec<PathBuf>,
        /// Skip content hashing (faster on slow disks; no exact-duplicate detection).
        #[arg(long)]
        no_hash: bool,
        /// Include only these kinds of file, e.g. `--types jpeg` or `--types jpeg,raw`
        /// (remembered for rescans; default: the folder's earlier choice, or all).
        #[arg(long, value_delimiter = ',', value_enum)]
        types: Option<Vec<KindArg>>,
    },
    /// Refresh every folder previously scanned.
    Rescan {
        #[arg(long)]
        no_hash: bool,
    },
    /// Search. Examples: `beach`, `make:canon iso:>1600`, `year:[2019 TO 2021] rating:>=4`.
    Search {
        /// Query; omit to match everything.
        query: Vec<String>,
        /// Match the words by what the photos show, e.g. `--smart dog on a beach year:2024`
        /// (run `analyze` first). Field terms still filter; results are ranked by match.
        #[arg(long)]
        smart: bool,
        /// List every photo instead of one per stack (copies, RAF+JPG pairs, versions, bursts).
        #[arg(long)]
        unstacked: bool,
        #[arg(long, value_enum, default_value_t = SortArg::Relevance)]
        sort: SortArg,
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: usize,
        #[arg(long, default_value_t = 0)]
        offset: usize,
    },
    /// Count matches per value: camera, lenses, date, ext, tags, folder.
    Facets {
        field: String,
        /// Only count photos matching this query.
        #[arg(short, long, default_value = "")]
        query: String,
        /// Drill down, e.g. `/Canon` for camera or `/2023` for date.
        #[arg(short, long, default_value = "/")]
        parent: String,
    },
    /// Show everything known about a photo (by id or path).
    Show { photo: String },
    /// Rate a photo 1-5, or 0 to clear.
    Rate {
        photo: String,
        #[arg(value_parser = clap::value_parser!(u8).range(0..=5))]
        rating: u8,
    },
    /// Add or remove tags.
    Tag {
        photo: String,
        #[arg(short, long)]
        add: Vec<String>,
        #[arg(short, long)]
        remove: Vec<String>,
    },
    /// Rebuild the search index from the catalog.
    Reindex,
    /// Library size and location.
    Stats,
    /// Find faces in photos not yet analyzed (downloads the face models once, ~39 MB).
    Faces {
        /// Photos per batch.
        #[arg(long, default_value_t = 150)]
        batch: usize,
    },
    /// Analyze photos for smart search (downloads the SigLIP models once, ~815 MB).
    Analyze {
        /// Photos per batch.
        #[arg(long, default_value_t = 200)]
        batch: usize,
    },
    /// List named people and unnamed groups of similar faces.
    People,
    /// Name faces (ids from `people` or `show-faces`), creating the person if new.
    Name {
        name: String,
        #[arg(required = true)]
        faces: Vec<i64>,
    },
    /// List the faces found in a photo.
    ShowFaces { photo: String },
    /// Show or change library settings.
    Settings {
        /// Keep ratings, tags and people in `photo.jpg.xmp` sidecars next to the photos
        /// (on by default). Sidecars changed by other apps are always read.
        #[arg(long, value_enum)]
        sidecars: Option<OnOff>,
        /// Score photos with the LAION aesthetic predictor (downloads 1.2 GB once;
        /// `analyze` then scores every photo, about 0.35 s each).
        #[arg(long, value_enum)]
        aesthetic: Option<OnOff>,
    },
    /// Read JSON commands from stdin, one per line; write one JSON response per line.
    Exec,
}

#[derive(Clone, Copy, ValueEnum)]
enum OnOff {
    On,
    Off,
}

#[derive(Clone, Copy, ValueEnum)]
enum KindArg {
    /// .jpg, .jpeg
    Jpeg,
    /// camera raw: .raf, .dng, .nef, .cr2, .cr3, .arw, ...
    Raw,
    /// .heic, .heif, .avif
    Heif,
    /// .png, .tif, .tiff, .webp
    Other,
}

impl KindArg {
    fn kind(self) -> FileKind {
        match self {
            KindArg::Jpeg => FileKind::Jpeg,
            KindArg::Raw => FileKind::Raw,
            KindArg::Heif => FileKind::Heif,
            KindArg::Other => FileKind::Other,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum SortArg {
    Relevance,
    Newest,
    Oldest,
    /// Your learned taste once you've rated enough photos, else the aesthetic score.
    Best,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_env("TINTYPE_LOG"))
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let library = match cli.library {
        Some(dir) => dir,
        None => tintype_engine::default_library_dir().context("no home folder; pass --library")?,
    };
    let mut engine = Engine::open(&library).with_context(|| format!("opening library {}", library.display()))?;

    let cmd = match cli.cmd {
        Cmd::Scan { folders, no_hash, types } => Command::Scan {
            roots: folders,
            hash: !no_hash,
            kinds: types.map(|t| t.into_iter().map(KindArg::kind).collect()),
        },
        Cmd::Rescan { no_hash } => Command::Rescan { hash: !no_hash },
        Cmd::Search { query, smart, unstacked, sort, limit, offset } => {
            let sort = match sort {
                SortArg::Relevance => Sort::Relevance,
                SortArg::Newest => Sort::Newest,
                SortArg::Oldest => Sort::Oldest,
                SortArg::Best => Sort::Best,
            };
            Command::Search { query: query.join(" "), smart, stacks: !unstacked, sort, limit, offset }
        }
        Cmd::Facets { field, query, parent } => Command::Facets { field, query, parent },
        Cmd::Show { photo } => Command::Show { photo: photo_ref(&photo) },
        Cmd::Rate { photo, rating } => {
            Command::Rate { photo: photo_ref(&photo), rating: (rating > 0).then_some(rating) }
        }
        Cmd::Tag { photo, add, remove } => Command::Tag { photo: photo_ref(&photo), add, remove },
        Cmd::Reindex => Command::Reindex,
        Cmd::Stats => Command::Stats,
        Cmd::Exec => return exec(&mut engine),
        Cmd::Faces { batch } => return find_faces(&mut engine, batch, cli.json),
        Cmd::Analyze { batch } => return analyze(&mut engine, batch, cli.json),
        Cmd::People => Command::People,
        Cmd::Name { name, faces } => Command::NameFaces { faces, name },
        Cmd::ShowFaces { photo } => Command::PhotoFaces { photo: photo_ref(&photo) },
        Cmd::Settings { sidecars, aesthetic } => Command::Settings {
            write_sidecars: sidecars.map(|s| matches!(s, OnOff::On)),
            aesthetic: aesthetic.map(|s| matches!(s, OnOff::On)),
        },
    };

    let response = engine.execute(cmd, &mut progress)?;
    if std::io::stderr().is_terminal() {
        eprint!("\r\x1b[2K");
    }
    if cli.json {
        println!("{}", serde_json::to_string_pretty(&response)?);
    } else {
        print_response(&response);
    }
    Ok(())
}

/// A number is an id unless a file by that name exists.
fn photo_ref(s: &str) -> PhotoRef {
    match s.parse() {
        Ok(id) if !Path::new(s).exists() => PhotoRef::Id(id),
        _ => PhotoRef::Path(s.into()),
    }
}

fn progress(event: Event) {
    let mut err = std::io::stderr();
    if !err.is_terminal() {
        return;
    }
    let line = match event {
        Event::Scan(ScanProgress::Walking { found }) => format!("finding photos... {found}"),
        Event::Scan(ScanProgress::Analyzing { done, total }) => {
            format!("reading metadata {done}/{total}")
        }
        Event::Reindex { done, total } => format!("indexing {done}/{total}"),
        Event::Download { model, done, total } => {
            format!("downloading {model}: {:.0}%", done as f64 * 100.0 / total.max(1) as f64)
        }
        Event::Faces { done, total } => format!("finding faces {done}/{total}"),
        Event::Analyze { done, total } => format!("analyzing {done}/{total}"),
    };
    let _ = write!(err, "\r\x1b[2K{line}");
    let _ = err.flush();
}

/// Runs face batches until every photo has been analyzed.
fn find_faces(engine: &mut Engine, batch: usize, json: bool) -> Result<()> {
    let started = std::time::Instant::now();
    let (mut photos, mut faces, mut failed) = (0, 0, 0);
    loop {
        let Response::Faces(r) = engine.execute(Command::FindFaces { batch }, &mut progress)? else {
            unreachable!("FindFaces answers with Faces")
        };
        (photos, faces, failed) = (photos + r.processed, faces + r.faces, failed + r.failed);
        if std::io::stderr().is_terminal() {
            eprint!("\r\x1b[2K{photos} photos analyzed, {faces} faces, {} to go", r.remaining);
        }
        if r.remaining == 0 || r.cancelled || r.processed == 0 {
            break;
        }
    }
    if std::io::stderr().is_terminal() {
        eprint!("\r\x1b[2K");
    }
    let secs = started.elapsed().as_secs_f64();
    if json {
        println!("{}", serde_json::json!({ "photos": photos, "faces": faces, "failed": failed, "seconds": secs }));
    } else {
        println!("{photos} photos analyzed in {secs:.1}s: {faces} faces found");
        if failed > 0 {
            println!("{failed} photos could not be decoded (no preview support for their format yet)");
        }
    }
    Ok(())
}

/// Runs analysis batches until every photo has an embedding.
fn analyze(engine: &mut Engine, batch: usize, json: bool) -> Result<()> {
    let started = std::time::Instant::now();
    let (mut photos, mut failed) = (0, 0);
    loop {
        let Response::Analyze(r) = engine.execute(Command::Analyze { batch }, &mut progress)? else {
            unreachable!("Analyze answers with Analyze")
        };
        (photos, failed) = (photos + r.processed, failed + r.failed);
        if std::io::stderr().is_terminal() {
            eprint!("\r\x1b[2K{photos} photos analyzed, {} to go", r.remaining);
        }
        if r.remaining == 0 || r.cancelled || r.processed == 0 {
            break;
        }
    }
    if std::io::stderr().is_terminal() {
        eprint!("\r\x1b[2K");
    }
    let secs = started.elapsed().as_secs_f64();
    if json {
        println!("{}", serde_json::json!({ "photos": photos, "failed": failed, "seconds": secs }));
    } else {
        println!("{photos} photos analyzed in {secs:.1}s");
        if failed > 0 {
            println!("{failed} photos could not be decoded (no preview support for their format yet)");
        }
    }
    Ok(())
}

fn exec(engine: &mut Engine) -> Result<()> {
    let mut out = std::io::stdout().lock();
    for line in std::io::stdin().lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Command>(&line) {
            Ok(cmd) => match engine.execute(cmd, &mut |_| {}) {
                Ok(r) => serde_json::to_value(&r)?,
                Err(e) => serde_json::json!({ "response": "error", "message": e.to_string() }),
            },
            Err(e) => {
                serde_json::json!({ "response": "error", "message": format!("bad command: {e}") })
            }
        };
        writeln!(out, "{reply}")?;
        out.flush()?;
    }
    Ok(())
}

fn print_response(response: &Response) {
    match response {
        Response::Scan(r) => {
            println!(
                "{} photos found in {:.1}s: {} new, {} changed, {} unchanged, {} moved, {} removed",
                r.found,
                r.elapsed_ms as f64 / 1000.0,
                r.added,
                r.updated,
                r.unchanged,
                r.moved,
                r.removed
            );
            let s = &r.sidecars;
            if s.imported + s.written + s.failed > 0 {
                println!("sidecars: {} read, {} written, {} failed", s.imported, s.written, s.failed);
            }
            if r.unreadable > 0 {
                println!("{} photos had unreadable metadata (cataloged by path only)", r.unreadable);
            }
            if r.skipped > 0 {
                println!("{} files or folders could not be accessed", r.skipped);
            }
            if r.cancelled {
                println!("scan was cancelled; run it again to finish");
            }
        }
        Response::Search(r) => {
            for hit in &r.hits {
                match hit.stack {
                    Some(stack) if hit.stack_size > 1 => {
                        println!("{:>7}  {}  (+{} more: stack:{stack})", hit.id, hit.path, hit.stack_size - 1)
                    }
                    _ => println!("{:>7}  {}", hit.id, hit.path),
                }
            }
            let shown = r.hits.len();
            if shown < r.total {
                println!("({shown} of {} matches; use -n / --offset for more)", r.total);
            } else {
                println!("({} matches)", r.total);
            }
        }
        Response::Facets { counts } => {
            for c in counts {
                println!("{:>7}  {}", c.count, c.path);
            }
        }
        Response::Photo(p) => print_photo(p),
        Response::Reindexed { photos } => println!("indexed {photos} photos"),
        Response::Faces(r) => println!("{} photos analyzed, {} faces, {} to go", r.processed, r.faces, r.remaining),
        Response::Analyze(r) => println!("{} photos analyzed, {} to go", r.processed, r.remaining),
        Response::People { people, groups, unscanned, .. } => {
            if people.is_empty() {
                println!("No named people yet.");
            }
            for p in people {
                let suggested = if p.suggested > 0 { format!(" (+{} suggested)", p.suggested) } else { String::new() };
                println!("{:>5}  {}: {} faces{suggested} in {} photos", p.id, p.name, p.faces, p.photos);
            }
            if !groups.is_empty() {
                println!("\nUnnamed groups (name one with `tintype-cli name NAME FACE...`):");
                for g in groups.iter().take(30) {
                    let ids: Vec<String> = g.faces.iter().take(8).map(|f| f.to_string()).collect();
                    println!("  {} faces: {}{}", g.size, ids.join(" "), if g.size > 8 { " ..." } else { "" });
                }
                if groups.len() > 30 {
                    println!("  ... and {} more groups", groups.len() - 30);
                }
            }
            if *unscanned > 0 {
                println!("\n{unscanned} photos not yet analyzed; run `tintype-cli faces`.");
            }
        }
        Response::FaceList { faces } => {
            for f in faces {
                let who = match (&f.person, f.confirmed) {
                    (Some(n), true) => n.clone(),
                    (Some(n), false) => format!("{n}?"),
                    (None, _) => "unknown".into(),
                };
                let [x, y, w, h] = f.bbox;
                println!(
                    "{:>7}  {who:<20} at {:.0}%,{:.0}% size {:.0}%x{:.0}%",
                    f.id,
                    x * 100.0,
                    y * 100.0,
                    w * 100.0,
                    h * 100.0
                );
            }
        }
        Response::Changed { photos } => println!("updated {photos} photos"),
        Response::Sidecars(s) => println!("sidecars: {} read, {} written, {} failed", s.imported, s.written, s.failed),
        Response::Settings { write_sidecars, aesthetic, aesthetic_download_bytes, sidecars } => {
            let state = if *write_sidecars { "on" } else { "off (read only)" };
            println!("sidecars  {state}");
            let download = match aesthetic_download_bytes {
                0 => String::new(),
                b => format!(" ({} MB to download on the next `analyze`)", b / 1_000_000),
            };
            println!("aesthetic {}{download}", if *aesthetic { "on" } else { "off" });
            if let Some(s) = sidecars {
                println!("caught up: {} sidecars written, {} read, {} failed", s.written, s.imported, s.failed);
            }
        }
        Response::Stats(s) => {
            println!("library  {}", s.library);
            println!("photos   {}", s.photos);
            println!("indexed  {}", s.indexed);
            for (f, kinds) in s.folders.iter().zip(&s.folder_kinds) {
                match kinds {
                    Some(k) => {
                        let names: Vec<&str> = k.iter().map(|k| k.name()).collect();
                        println!("folder   {f}  (only {})", names.join(", "));
                    }
                    None => println!("folder   {f}"),
                }
            }
            if s.unanalyzed > 0 {
                println!("\n{} photos not yet analyzed for smart search; run `tintype-cli analyze`.", s.unanalyzed);
            }
            if s.taste_ratings < s.taste_needed {
                println!(
                    "{} of {} ratings made so far; then Tintype learns your taste (sort best).",
                    s.taste_ratings, s.taste_needed
                );
            } else {
                println!("taste    learned from your {} ratings", s.taste_ratings);
            }
        }
    }
}

fn print_photo(p: &PhotoRecord) {
    let m = &p.meta;
    let row = |label: &str, value: Option<String>| {
        if let Some(v) = value {
            println!("{label:<12} {v}");
        }
    };
    row("id", Some(p.id.to_string()));
    row("path", Some(p.path.clone()));
    row("size", Some(format!("{:.1} MB", p.size as f64 / 1_048_576.0)));
    row("dimensions", m.width.zip(m.height).map(|(w, h)| format!("{w} x {h}")));
    row("taken", m.taken.map(|t| t.to_string()));
    row(
        "camera",
        match (&m.make, &m.model) {
            (Some(make), Some(model)) if model.starts_with(make.as_str()) => Some(model.clone()),
            (Some(make), Some(model)) => Some(format!("{make} {model}")),
            (a, b) => a.clone().or(b.clone()),
        },
    );
    row("lens", m.lens.clone());
    let exposure = [
        m.f_number.map(|f| format!("f/{f}")),
        m.exposure_s.map(|s| if s < 1.0 { format!("1/{:.0}s", 1.0 / s) } else { format!("{s}s") }),
        m.iso.map(|i| format!("ISO {i}")),
        m.focal_mm.map(|f| format!("{f}mm")),
    ];
    let exposure: Vec<_> = exposure.into_iter().flatten().collect();
    row("exposure", (!exposure.is_empty()).then(|| exposure.join("  ")));
    row("gps", m.gps.map(|g| format!("{:.6}, {:.6}", g.lat, g.lon)));
    row("rating", p.rating.map(|r| "★".repeat(r.into())));
    row("tags", (!p.tags.is_empty()).then(|| p.tags.join(", ")));
    row("faces", (p.faces > 0).then(|| p.faces.to_string()));
    row("people", (!p.people.is_empty()).then(|| p.people.join(", ")));
    row("stack", p.stack.map(|s| format!("{s}{}", if p.stack_cover { " (shown for the stack)" } else { "" })));
    row("aesthetic", p.aesthetic.map(|a| format!("{a:.2}")));
    row("suggested", p.suggested_rating.map(|r| format!("{r:.1} stars, from your ratings")));
    row("hash", p.hash.clone());
    row("error", p.error.clone());
    if !m.fields.is_empty() {
        println!("\nEXIF");
        for (k, v) in &m.fields {
            println!("  {k:<28} {v}");
        }
    }
}
