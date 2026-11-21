//! Port of Go `internal/store/profilegen/cmd/profilegen`: regenerates the committed Store
//! profile from the pinned Apple binaries under Unicorn. Normal builds do not run it.
//!
//! Go writes `data_generated.go`; the Rust profile embeds the three zstd frames in
//! `crates/ipatool-store/profile/`, so `--output` names that directory. A profile whose decoded
//! artifacts already match is left untouched, which keeps the frames Go produced.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;
use ipatool_profilegen::{Artifacts, compress, generate, version};
use ipatool_sap::assets::StoreCache;
use ipatool_store::profile::PINNED_INPUTS;

#[derive(Parser)]
#[command(about = "Regenerate the Store profile from pinned Apple binaries under Unicorn")]
struct Arguments {
    /// Optional offline copy of the pinned dyld shared cache (the primary file; members
    /// are found by suffix). Without it the cache is read from Apple's restore image.
    #[arg(long)]
    cache: Option<PathBuf>,
    /// Directory of the embedded profile artifacts.
    #[arg(long, default_value = "crates/ipatool-store/profile")]
    output: PathBuf,
    /// Regenerate and compare with the committed artifacts without writing files.
    #[arg(long)]
    check: bool,
}

const FILES: [&str; 3] = ["metadata.json.zst", "pages.bin.zst", "plugin.zst"];

fn main() -> ExitCode {
    match run(&Arguments::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn run(arguments: &Arguments) -> Result<(), String> {
    eprintln!("Generating Store profile from pinned Apple binaries under Unicorn...");
    let artifacts = match &arguments.cache {
        Some(path) => {
            let cache = StoreCache::open_local(path)
                .map_err(|error| format!("open profile inputs: {error}"))?;
            artifacts(&cache)?
        }
        None => StoreCache::with_remote(|cache| Ok(artifacts(cache)))
            .map_err(|error| format!("open profile inputs: {error}"))??,
    };
    let artifacts = [artifacts.metadata, artifacts.pages, artifacts.plugin];

    let current = FILES.map(|name| read_artifact(&arguments.output.join(name)));
    let changed: Vec<bool> = current
        .iter()
        .zip(&artifacts)
        .map(|(current, generated)| current.as_ref().ok() != Some(generated))
        .collect();
    let version = version();
    if !changed.contains(&true) && version == PINNED_INPUTS {
        eprintln!("Store profile is up to date.");
        return Ok(());
    }
    for (name, current) in FILES.iter().zip(&current) {
        if let Err(error) = current
            && error.kind() != std::io::ErrorKind::NotFound
        {
            return Err(format!("read generated profile {name}: {error}"));
        }
    }
    if arguments.check {
        return Err(format!(
            "generated Store profile differs from {}; regenerate it before committing",
            arguments.output.display()
        ));
    }
    for ((name, data), changed) in FILES.iter().zip(&artifacts).zip(changed) {
        if changed {
            let path = arguments.output.join(name);
            let compressed = compress(data);
            write_atomically(&path, &compressed)?;
            eprintln!("Wrote {} ({} bytes).", path.display(), compressed.len());
        }
    }
    // The loader in `ipatool_store::profile` pins each decoded size and digest.
    for (name, data) in FILES.iter().zip(&artifacts) {
        eprintln!("{name}: size {} sha256 {}", data.len(), hex_digest(data));
    }
    if version != PINNED_INPUTS {
        eprintln!("Pinned inputs: {version}");
    }
    Ok(())
}

fn artifacts(cache: &StoreCache<'_>) -> Result<Artifacts, String> {
    let result = generate(cache).map_err(|error| format!("generate profile: {error}"))?;
    let plugin = cache
        .extract_authentication_plugin()
        .map_err(|error| format!("extract authentication plugin: {error}"))?;
    result
        .artifacts(plugin)
        .map_err(|error| format!("encode profile source: {error}"))
}

fn read_artifact(path: &Path) -> std::io::Result<Vec<u8>> {
    let compressed = std::fs::read(path)?;
    let mut decoder = ruzstd::decoding::StreamingDecoder::new(compressed.as_slice())
        .map_err(std::io::Error::other)?;
    let mut data = Vec::new();
    decoder.read_to_end(&mut data)?;
    Ok(data)
}

/// Go's `writeSource`: the file is replaced only after it is completely written.
fn write_atomically(path: &Path, data: &[u8]) -> Result<(), String> {
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(directory)
        .map_err(|error| format!("create profile output directory: {error}"))?;
    let temporary = directory.join(format!(
        ".{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    let write = || -> std::io::Result<()> {
        let file = std::fs::File::create(&temporary)?;
        std::io::Write::write_all(&mut &file, data)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)
    };
    write().map_err(|error| {
        let _ = std::fs::remove_file(&temporary);
        format!("write profile artifact {}: {error}", path.display())
    })
}

fn hex_digest(data: &[u8]) -> String {
    use sha2::Digest as _;
    ipatool_profilegen::hex(&sha2::Sha256::digest(data))
}
