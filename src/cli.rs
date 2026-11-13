use crate::{
    config::{self, AppConfig},
    database::Database,
    indexer::{self, ScanEvent},
    model::{ExportFormat, FileEntry, SearchOptions, SearchRequest, Sort, SortColumn},
    operations,
};
use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use std::{path::PathBuf, sync::atomic::AtomicBool, time::Instant};

#[derive(Parser)]
#[command(
    version,
    about = "Búsqueda local de archivos en Rust",
    long_about = "Everything Rust: índice persistente, búsqueda avanzada y aplicación de escritorio. Use es.exe para la consola en Windows."
)]
struct Cli {
    #[arg(
        long,
        global = true,
        help = "Carpeta dedicada al índice y la configuración"
    )]
    data_dir: Option<PathBuf>,
    #[arg(
        long,
        global = true,
        help = "Guarda los datos en ./data junto al ejecutable"
    )]
    portable: bool,
    #[arg(
        long,
        global = true,
        help = "Raíz a indexar; repetir para varias unidades o carpetas"
    )]
    root: Vec<PathBuf>,
    #[arg(
        long,
        global = true,
        help = "Ruta excluida; repetir para varias carpetas"
    )]
    exclude: Vec<PathBuf>,
    #[command(subcommand)]
    command: Option<Action>,
}

#[derive(Subcommand)]
enum Action {
    #[command(about = "Abre la interfaz gráfica")]
    Gui {
        #[arg(long)]
        query: Option<String>,
    },
    #[command(about = "Actualiza el índice (un recorrido, sin servicio permanente)")]
    Index,
    #[command(about = "Busca en el índice existente")]
    Search {
        #[arg(default_value = "")]
        query: String,
        #[arg(long, default_value_t = 500)]
        limit: usize,
        #[arg(long, default_value_t = 0)]
        offset: u64,
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        options: QueryArgs,
    },
    #[command(about = "Exporta todos los resultados a CSV o EFU")]
    Export {
        #[arg(default_value = "")]
        query: String,
        #[arg(short, long)]
        output: PathBuf,
        #[arg(long)]
        efu: bool,
        #[command(flatten)]
        options: QueryArgs,
    },
    #[command(about = "Importa un catálogo EFU sin conexión")]
    Import { path: PathBuf },
    #[command(about = "Muestra estadísticas y comprueba integridad si se solicita")]
    Stats {
        #[arg(long)]
        check: bool,
    },
    #[command(about = "Mide un índice sintético aislado; no modifica su índice")]
    Benchmark {
        #[arg(long, default_value_t = 100_000)]
        records: u64,
    },
}

#[derive(clap::Args, Default)]
struct QueryArgs {
    #[arg(long)]
    case_sensitive: bool,
    #[arg(long)]
    match_path: bool,
    #[arg(long)]
    whole_word: bool,
    #[arg(long)]
    regex: bool,
    #[arg(long, value_enum, default_value_t = Order::Name)]
    sort: Order,
    #[arg(long)]
    descending: bool,
}

#[derive(Clone, Copy, Default, ValueEnum)]
enum Order {
    #[default]
    Name,
    Path,
    Size,
    Modified,
    Created,
    Extension,
}

impl QueryArgs {
    fn request(&self, text: String, offset: u64, limit: usize) -> SearchRequest {
        SearchRequest {
            text,
            offset,
            limit,
            options: SearchOptions {
                case_sensitive: self.case_sensitive,
                match_path: self.match_path,
                whole_word: self.whole_word,
                regex: self.regex,
            },
            sort: Sort {
                column: match self.sort {
                    Order::Name => SortColumn::Name,
                    Order::Path => SortColumn::Path,
                    Order::Size => SortColumn::Size,
                    Order::Modified => SortColumn::Modified,
                    Order::Created => SortColumn::Created,
                    Order::Extension => SortColumn::Extension,
                },
                descending: self.descending,
            },
            ..SearchRequest::default()
        }
    }
}

pub fn run(default_gui: bool) -> Result<()> {
    let args = Cli::parse();
    if let Some(Action::Benchmark { records }) = args.command {
        return benchmark(records);
    }
    let data_dir = if let Some(path) = args.data_dir {
        std::path::absolute(path)?
    } else if args.portable {
        std::env::current_exe()?
            .parent()
            .context("No se encontró la carpeta del ejecutable")?
            .join("data")
    } else {
        config::data_directory()?
    };
    std::fs::create_dir_all(&data_dir)?;
    let data_dir = std::fs::canonicalize(data_dir)?;
    let db_path = data_dir.join("index.db");
    let config_path = data_dir.join("settings.json");
    let mut config = AppConfig::load(&config_path)?;
    if !args.root.is_empty() {
        config.roots = args
            .root
            .iter()
            .map(std::path::absolute)
            .collect::<std::io::Result<Vec<_>>>()?;
    }
    if !args.exclude.is_empty() {
        config.excludes = args
            .exclude
            .iter()
            .map(std::path::absolute)
            .collect::<std::io::Result<Vec<_>>>()?;
    }
    config = crate::engine::normalize_config(config, &db_path)?;
    let action = args.command.unwrap_or(if default_gui {
        Action::Gui { query: None }
    } else {
        Action::Stats { check: false }
    });
    match action {
        Action::Gui { query } => {
            #[cfg(feature = "gui")]
            {
                let engine = crate::engine::Engine::start(db_path, config_path, config.clone())?;
                crate::gui::run(engine, config, query)?;
            }
            #[cfg(not(feature = "gui"))]
            {
                let _ = query;
                bail!(
                    "Este ejecutable se compiló sin interfaz. Use es index/search o compile con la característica gui."
                );
            }
        }
        Action::Index => {
            let _lease = crate::lease::WriterLease::acquire(&db_path)?;
            for root in &config.roots {
                if root.starts_with(&data_dir) {
                    bail!("La carpeta de datos debe estar separada de la raíz que se indexa");
                }
            }
            config.save(&config_path)?;
            let mut db = Database::open(&db_path)?;
            db.remove_unconfigured_roots(
                &config
                    .roots
                    .iter()
                    .map(|root| root.to_string_lossy().into_owned())
                    .collect::<Vec<_>>(),
            )?;
            let generation = db.next_generation()?;
            let mut excludes = config.excludes.clone();
            excludes.push(data_dir);
            for excluded in &excludes {
                db.remove_subtree(&excluded.to_string_lossy())?;
            }
            let start = Instant::now();
            let summary =
                indexer::scan(&config.roots, &excludes, &AtomicBool::new(false), |event| {
                    match event {
                        ScanEvent::Batch { root, entries } => {
                            db.upsert_batch(&root.to_string_lossy(), generation, &entries)?
                        }
                        ScanEvent::Progress {
                            scanned,
                            errors,
                            current,
                        } => eprintln!("{scanned} entradas, {errors} avisos · {current}"),
                        ScanEvent::Warning(message) => eprintln!("Aviso: {message}"),
                    }
                    Ok(())
                })?;
            if !summary.cancelled {
                for root in summary.complete_roots {
                    db.prune_root(&root.to_string_lossy(), generation)?;
                }
                for (root, protected) in summary.incomplete_roots {
                    db.prune_root_except(&root.to_string_lossy(), generation, &protected)?;
                }
            }
            db.checkpoint()?;
            println!(
                "{} entradas indexadas; {} errores de acceso; {:.2} s",
                db.total()?,
                summary.errors,
                start.elapsed().as_secs_f64()
            );
        }
        Action::Search {
            query,
            limit,
            offset,
            json,
            options,
        } => {
            let db = Database::open(&db_path)?;
            let page = db.search(&options.request(query, offset, limit))?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &serde_json::json!({"total":page.total,"offset":page.offset,"elapsed_ms":page.elapsed_ms,"rows":page.rows})
                    )?
                );
            } else {
                for row in &page.rows {
                    println!("{}", row.path);
                }
                eprintln!(
                    "{} de {} resultados · {} ms",
                    page.rows.len(),
                    page.total,
                    page.elapsed_ms
                );
            }
        }
        Action::Export {
            query,
            output,
            efu,
            options,
        } => {
            let format = if efu
                || output
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("efu"))
            {
                ExportFormat::Efu
            } else {
                ExportFormat::Csv
            };
            let count = operations::export(
                &db_path,
                &output,
                &options.request(query, 0, 500),
                format,
                &AtomicBool::new(false),
            )?;
            println!("{count} resultados exportados a {}", output.display());
        }
        Action::Import { path } => {
            let _lease = crate::lease::WriterLease::acquire(&db_path)?;
            println!(
                "{} registros importados como catálogo sin conexión",
                operations::import(&db_path, &path, &AtomicBool::new(false))?
            );
        }
        Action::Stats { check } => {
            let db = Database::open(&db_path)?;
            if check {
                db.integrity_check()?;
            }
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &serde_json::json!({"entries":db.total()?,"database":db_path,"database_bytes":std::fs::metadata(&db_path)?.len(),"integrity_checked":check,"roots":config.roots,"exclusions":config.excludes})
                )?
            );
        }
        Action::Benchmark { .. } => unreachable!(),
    }
    Ok(())
}

fn benchmark(records: u64) -> Result<()> {
    if !(1000..=10_000_000).contains(&records) {
        bail!("Use entre 1.000 y 10.000.000 registros");
    }
    let temp = tempfile::tempdir()?;
    let db_path = temp.path().join("benchmark.db");
    let mut db = Database::open(&db_path)?;
    let start = Instant::now();
    let mut batch = Vec::with_capacity(1024);
    for i in 0..records {
        let extension = ["pdf", "rs", "txt", "png"][i as usize % 4];
        let stem = if i % 10 == 0 { "invoice" } else { "document" };
        let name = format!("{stem}_{i:08}.{extension}");
        let parent = format!("/benchmark/project_{:04}/data", i % 1000);
        batch.push(FileEntry {
            path: format!("{parent}/{name}"),
            name,
            parent,
            extension: extension.into(),
            size: Some((i % 500_000) * 1024),
            modified: Some(1_700_000_000 + i as i64),
            ..FileEntry::default()
        });
        if batch.len() == 1024 {
            db.upsert_batch("/benchmark", 1, &batch)?;
            batch.clear();
        }
    }
    if !batch.is_empty() {
        db.upsert_batch("/benchmark", 1, &batch)?;
    }
    db.checkpoint()?;
    let indexing_seconds = start.elapsed().as_secs_f64();
    let mut results = Vec::new();
    for text in [
        "invoice_00000100",
        "ext:pdf",
        "project_0042",
        "size:>100MiB ext:png",
        "document !ext:txt",
        "",
    ] {
        let options = SearchOptions {
            match_path: text.starts_with("project"),
            ..SearchOptions::default()
        };
        let request = SearchRequest {
            text: text.into(),
            options,
            ..SearchRequest::default()
        };
        let mut timings = Vec::new();
        let mut matches = 0;
        for _ in 0..5 {
            let started = Instant::now();
            let page = db.search(&request)?;
            matches = page.total;
            timings.push(started.elapsed().as_secs_f64() * 1000.0);
        }
        timings.sort_by(f64::total_cmp);
        results.push(serde_json::json!({"query":text,"match_path":options.match_path,"total_matches":matches,"median_ms":timings[2],"max_ms":timings[4],"page_limit":500}));
    }
    println!(
        "{}",
        serde_json::to_string_pretty(
            &serde_json::json!({"kind":"synthetic_sqlite_index_not_filesystem_scan","records":records,"platform":std::env::consts::OS,"architecture":std::env::consts::ARCH,"logical_cpus":std::thread::available_parallelism()?.get(),"build_profile":if cfg!(debug_assertions){"debug"}else{"release"},"indexing_seconds":indexing_seconds,"records_per_second":records as f64/indexing_seconds,"database_bytes":std::fs::metadata(&db_path)?.len(),"searches":results})
        )?
    );
    Ok(())
}
