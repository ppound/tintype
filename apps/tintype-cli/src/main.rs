//! `tintype-cli`: the command line. Every subcommand maps to one engine
//! [`Command`]; `tintype exec` reads commands as JSON lines from stdin.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use tintype_engine::{Command, Engine, Event, PhotoRecord, PhotoRef, Response, ScanProgress, Sort};

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
    /// Read JSON commands from stdin, one per line; write one JSON response per line.
    Exec,
}

#[derive(Clone, Copy, ValueEnum)]
enum SortArg {
    Relevance,
    Newest,
    Oldest,
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
        Cmd::Scan { folders, no_hash } => Command::Scan { roots: folders, hash: !no_hash },
        Cmd::Rescan { no_hash } => Command::Rescan { hash: !no_hash },
        Cmd::Search { query, sort, limit, offset } => {
            let sort = match sort {
                SortArg::Relevance => Sort::Relevance,
                SortArg::Newest => Sort::Newest,
                SortArg::Oldest => Sort::Oldest,
            };
            Command::Search { query: query.join(" "), sort, limit, offset }
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
    };
    let _ = write!(err, "\r\x1b[2K{line}");
    let _ = err.flush();
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
                "{} photos found in {:.1}s: {} new, {} changed, {} unchanged, {} removed",
                r.found,
                r.elapsed_ms as f64 / 1000.0,
                r.added,
                r.updated,
                r.unchanged,
                r.removed
            );
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
                println!("{:>7}  {}", hit.id, hit.path);
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
        Response::Stats(s) => {
            println!("library  {}", s.library);
            println!("photos   {}", s.photos);
            println!("indexed  {}", s.indexed);
            for f in &s.folders {
                println!("folder   {f}");
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
    row("hash", p.hash.clone());
    row("error", p.error.clone());
    if !m.fields.is_empty() {
        println!("\nEXIF");
        for (k, v) in &m.fields {
            println!("  {k:<28} {v}");
        }
    }
}
