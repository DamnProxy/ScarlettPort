//! `victory`: the command-line front end.
//!
//! Every command reads the user's own files and writes only to stdout or to
//! the user's cache directory (outside the repository).

mod census;
mod inspect;
mod prewarm;
mod run;
mod saves;
mod shader;
mod translate;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};

// Agility SDK opt-in: Windows 10 loads D3D12Core.dll 1.614 from .\D3D12\
// next to the executable (the inbox runtime refuses HEAP_DIRECTLY_INDEXED).
#[repr(transparent)]
pub struct SdkPath(*const u8);
unsafe impl Sync for SdkPath {}

#[used]
#[unsafe(no_mangle)]
#[allow(non_upper_case_globals)]
pub static D3D12SDKVersion: u32 = 614;

#[used]
#[unsafe(no_mangle)]
#[allow(non_upper_case_globals)]
pub static D3D12SDKPath: SdkPath = SdkPath(c".\\D3D12\\".as_ptr().cast());

#[derive(Parser)]
#[command(
    name = "victory",
    version,
    about = "Run an Xbox Series title natively on Windows"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Summarise one PE image: headers, sections, imports, TLS, build id.
    Inspect {
        file: PathBuf,
        /// List every imported function.
        #[arg(long)]
        imports: bool,
        /// List every exported function.
        #[arg(long)]
        exports: bool,
    },
    /// Aggregate the imports of every PE image under the given paths and
    /// show which ones our runtime covers. Defaults to the configured
    /// game and test-title directories.
    Census {
        paths: Vec<PathBuf>,
        /// Do not count host pass-through as covered.
        #[arg(long)]
        strict: bool,
        /// List every missing `dll!function`, most used first.
        #[arg(long)]
        missing: bool,
    },
    /// List the `dll!export` pairs an import library describes.
    LibExports {
        lib: PathBuf,
        /// List every export, not just per-DLL counts.
        #[arg(long)]
        list: bool,
    },
    /// Find shader containers in files or directories (also inside engine
    /// archives, if uncompressed) and report their layout.
    ShaderInfo {
        paths: Vec<PathBuf>,
        /// Print one line per container found.
        #[arg(long)]
        verbose: bool,
        /// Also list the decoded native instructions (implies --verbose).
        #[arg(long)]
        disasm: bool,
    },
    /// Run every shader found (containers, SGD5 files) through the
    /// translator and report what it cannot handle yet.
    Translate {
        paths: Vec<PathBuf>,
        /// Check emitted DXIL with dxil.dll's validator (`VICTORY_DXIL`,
        /// else the newest Windows SDK's).
        #[arg(long)]
        validate: bool,
        /// Also create a host D3D12 pipeline for each emitted shader.
        #[arg(long)]
        pipelines: bool,
        /// Print one line per program and outcome.
        #[arg(long)]
        list: bool,
        /// Worker threads (default: every logical core).
        #[arg(long)]
        jobs: Option<usize>,
        /// Warm the run's shader cache: a file of shader hashes (the 16
        /// hex digits a run's log names a shader by, anywhere in a line);
        /// the containers with those hashes are translated into the
        /// cache and nothing else is done. Set `VICTORY_HOST_MAX_WAVE` and
        /// `VICTORY_VIEW_LIMIT` to what the run's log says of the host.
        #[arg(long)]
        warm: Option<PathBuf>,
    },
    /// Translate the shaders of a title's archives into the cache and make
    /// their compute pipelines, so that a run finds them there.
    Warm {
        paths: Vec<PathBuf>,
        /// Worker threads (default: every logical core).
        #[arg(long)]
        jobs: Option<usize>,
        /// Only translate: no host pipelines.
        #[arg(long)]
        no_pipelines: bool,
        /// Bring the cache up to date with this build's translator and say how much of it
        /// changed: entries of another translator build whose rules did not change are
        /// restamped, the others translated again and compared; a shader whose output is
        /// the same keeps its entry and its compiled pipelines.
        #[arg(long)]
        incremental: bool,
        /// With --incremental: translate every shader again whatever its entry's rule stamp
        /// says (the check on the stamps).
        #[arg(long, requires = "incremental")]
        verify_all: bool,
    },
    /// Make the host pipelines of the graphics records of the title's
    /// precompiled pipeline cache (in its shaders.rpf) into the pipeline
    /// library, at low priority, so that a run with
    /// VICTORY_PRECOMPILED_GRAPHICS=1 compiles none of them. Needs a GPU.
    PrecompileGraphics {
        /// The title's shaders.rpf (default: shaders.rpf of the configured
        /// game folder).
        archive: Option<PathBuf>,
        /// Worker threads (default: half the logical cores).
        #[arg(long)]
        jobs: Option<usize>,
        /// Only the first N records (after --skip).
        #[arg(long)]
        limit: Option<usize>,
        /// Leave out the first N records (of those chosen).
        #[arg(long, default_value_t = 0)]
        skip: usize,
        /// Only records with a mesh shader.
        #[arg(long)]
        mesh: bool,
    },
    /// First-boot prewarm: a manifest (hashes only) of what a warm cache holds, and making it
    /// into this machine's caches from its own game files.
    Prewarm {
        #[command(subcommand)]
        action: Prewarm,
    },
    /// Prepare, map and bind a title executable; with --execute, run it.
    Run {
        exe: PathBuf,
        /// What unimplemented imports do when called.
        #[arg(long, value_enum, default_value_t = Unimplemented::Trap)]
        unimplemented: Unimplemented,
        /// Disable host pass-through (everything must be implemented by us).
        #[arg(long)]
        strict: bool,
        /// Actually start the title. Without it, stop after binding.
        #[arg(long)]
        execute: bool,
        /// The console model the title sees (device type, memory).
        #[arg(long, value_enum, default_value_t = Console::SeriesX)]
        console: Console,
        /// Arguments for the title, after `--` (it reads the whole command
        /// line; a title that relaunches itself passes some).
        #[arg(last = true)]
        title_args: Vec<String>,
    },
    /// Show the resolved configuration and check it.
    Config,
    /// Manage save slots (`VICTORY_SAVE_SLOT=<name>` picks one when running).
    Saves {
        /// Title id (8 hex digits); default: every title with data.
        #[arg(long)]
        title: Option<String>,
        #[command(subcommand)]
        action: saves::Saves,
    },
}

#[derive(Subcommand)]
enum Prewarm {
    /// Write the manifest of the configured cache (it must be warm): the hashes of the
    /// containers of the game files it holds translations of, and the pipeline keys its runs
    /// asked for. Needs the host GPU the cache was warmed on (the keys depend on it).
    Manifest {
        /// Game files (default: the configured game folder's shaders.rpf).
        paths: Vec<PathBuf>,
        #[arg(long, short)]
        out: PathBuf,
        /// Worker threads (default: every logical core but one).
        #[arg(long)]
        jobs: Option<usize>,
        /// The cache was warmed by fx-cand, whose key names only its eight switches (fx-cand2
        /// names every switch of the translator, so it finds none of those entries).
        #[arg(long)]
        cand_key: bool,
    },
    /// Translate and compile what the manifest lists into the configured cache, at background
    /// priority; prints `PREWARM <phase> <done> <total>`. Stopping it keeps what was done.
    Run {
        /// Game files (default: the configured game folder's shaders.rpf).
        paths: Vec<PathBuf>,
        #[arg(long)]
        manifest: PathBuf,
        /// Worker threads (default: every logical core but one).
        #[arg(long)]
        jobs: Option<usize>,
        /// Make nothing: say what share of the manifest the next boot finds in the caches.
        #[arg(long)]
        check: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Unimplemented {
    Trap,
    Zero,
}

#[derive(Clone, Copy, ValueEnum)]
enum Console {
    SeriesX,
    SeriesS,
}

fn main() -> Result<()> {
    victory_core::env::alias_scarlett();
    victory_core::logging::init("warn");
    // The audio recorder `run` starts for `VICTORY_AUDIO_CAPTURE`.
    #[cfg(all(windows, feature = "diagnostics"))]
    if victory_hle::modules::audio::capture::child() {
        return Ok(());
    }
    let cli = Cli::parse();
    match cli.command {
        Command::Inspect {
            file,
            imports,
            exports,
        } => inspect::run(&file, imports, exports),
        Command::Census {
            paths,
            strict,
            missing,
        } => census::run(paths, strict, missing),
        Command::LibExports { lib, list } => inspect::lib_exports(&lib, list),
        Command::ShaderInfo {
            paths,
            verbose,
            disasm,
        } => shader::run(paths, verbose || disasm, disasm),
        Command::Translate {
            paths,
            validate,
            pipelines,
            list,
            jobs,
            warm,
        } => {
            // `VICTORY_VIEW_LIMIT=<null slot>`: translate as on a host whose view heap is capped
            if let Some(n) = std::env::var("VICTORY_VIEW_LIMIT").ok().and_then(|v| v.parse().ok()) {
                victory_shader::emit::set_view_heap_limit(n);
            }
            // `VICTORY_HOST_MAX_WAVE=32`: one name, read in one place (`translate::apply_host_max_wave`),
            // for the warm-up and the walk alike (adapted from Zettrox/ztrx#120)
            translate::apply_host_max_wave();
            if let Some(file) = warm {
                return translate::warm_listed(paths, &file, jobs);
            }
            translate::run(paths, validate, pipelines, list, jobs)
        }
        Command::PrecompileGraphics {
            archive,
            jobs,
            limit,
            skip,
            mesh,
        } => translate::precompile_graphics(archive, jobs, limit, skip, mesh),
        Command::Warm {
            paths,
            jobs,
            no_pipelines,
            incremental,
            verify_all,
        } => translate::warm(paths, jobs, !no_pipelines, incremental, verify_all),
        Command::Prewarm { action } => match action {
            Prewarm::Manifest { paths, out, jobs, cand_key } => prewarm::manifest(paths, &out, jobs, cand_key),
            Prewarm::Run { paths, manifest, jobs, check } => prewarm::run(paths, &manifest, jobs, check),
        },
        Command::Run {
            exe,
            unimplemented,
            strict,
            execute,
            console,
            title_args: _,
        } => run::run(&exe, unimplemented, strict, execute, console),
        Command::Config => show_config(),
        Command::Saves { title, action } => saves::run(action, title),
    }
}

fn show_config() -> Result<()> {
    let cfg = victory_core::Config::load()?;
    let status = |p: &Option<PathBuf>| match p {
        Some(p) if p.exists() => format!("{}", p.display()),
        Some(p) => format!("{}  (does not exist)", p.display()),
        None => "(not set)".to_owned(),
    };
    println!(
        "config file : {}",
        cfg.source
            .as_ref()
            .map_or("(none)".into(), |p| p.display().to_string())
    );
    println!("game_dir    : {}", status(&cfg.game_dir));
    println!("cache_dir   : {}", cfg.cache_dir.display());
    println!("data_dir    : {}", cfg.data_dir.display());
    for t in &cfg.test_titles {
        println!("test_title  : {}", status(&Some(t.clone())));
    }
    for p in cfg
        .game_dir
        .iter()
        .chain(&cfg.test_titles)
        .chain([&cfg.cache_dir, &cfg.data_dir])
    {
        if let Err(e) = victory_core::ensure_outside_repo(p) {
            println!("POLICY VIOLATION: {e}");
        }
    }
    Ok(())
}

/// Paths given on the command line, or the configured game + test titles.
pub(crate) fn default_paths(paths: Vec<PathBuf>) -> Result<Vec<PathBuf>> {
    if !paths.is_empty() {
        return Ok(paths);
    }
    let cfg = victory_core::Config::load()?;
    let all: Vec<PathBuf> = cfg.game_dir.into_iter().chain(cfg.test_titles).collect();
    anyhow::ensure!(
        !all.is_empty(),
        "no paths given and none configured (see victory.example.toml)"
    );
    Ok(all)
}
