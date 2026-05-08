use std::{
    io::{BufReader, BufWriter, Cursor, Write},
    path::PathBuf,
};

use byteorder::LittleEndian;
use clap::Parser;
use color_eyre::{
    Result,
    eyre::{Context, eyre},
};
use tracing_subscriber::{EnvFilter, fmt};
use unrealin::{
    ExportedData,
    de::LinearFileDecoder,
};

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Where to extract files to. By default this will be the basename of the input file.
    /// For example, `common.lin` will extract to `common/`
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Skip trace verification. Use this for `.lin` pairs we don't have
    /// a recorded I/O trace for (typical case: any map other than the
    /// menu replay we recorded `reads.json` against). Bootstrap loads
    /// only `MyLevel` and lets the dependency cascade pick up the rest.
    #[arg(long)]
    no_checked: bool,

    /// Common `.lin` (file_table holder).
    common_lin: PathBuf,

    /// Map `.lin` (level package).
    map_lin: PathBuf,
}

fn main() -> Result<()> {
    let mut args = Args::parse();

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    let subscriber = fmt().pretty().with_env_filter(filter).finish();
    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");

    let common_file = std::fs::File::open(&args.common_lin)
        .wrap_err_with(|| format!("failed to open {:?}", &args.common_lin))?;
    let common_mmap = unsafe { memmap2::Mmap::map(&common_file)? };
    let mut raw_common_file = &common_mmap[..];

    let map_file = std::fs::File::open(&args.map_lin)
        .wrap_err_with(|| format!("failed to open {:?}", &args.map_lin))?;
    let map_mmap = unsafe { memmap2::Mmap::map(&map_file)? };
    let mut raw_map_file = &map_mmap[..];

    let output_dir = if let Some(output_dir) = args.output.take() {
        output_dir
    } else {
        let Some(parent) = args.common_lin.parent() else {
            return Err(eyre!("Input path {:?} has no parent", args.common_lin));
        };

        let Some(stem) = args.common_lin.file_stem() else {
            return Err(eyre!("Input path {:?} has no file stem", args.common_lin));
        };

        parent.join(stem)
    };

    std::fs::create_dir_all(&output_dir)
        .wrap_err_with(|| format!("failed to create output dir {:?}", &output_dir))?;

    let common_lin_data = if args
        .common_lin
        .extension()
        .as_ref()
        .map(|ext| ext.to_str().unwrap() == "lin")
        .unwrap_or_default()
    {
        unrealin::de::decompress_linear_file::<LittleEndian, _>(&mut raw_common_file)?
    } else {
        raw_common_file.to_vec()
    };

    let map_lin_data = if args
        .map_lin
        .extension()
        .as_ref()
        .map(|ext| ext.to_str().unwrap() == "lin")
        .unwrap_or_default()
    {
        unrealin::de::decompress_linear_file::<LittleEndian, _>(&mut raw_map_file)?
    } else {
        raw_map_file.to_vec()
    };

    let pkg_out = output_dir.join("packages");
    std::fs::create_dir_all(&pkg_out)?;

    eprintln!(
        "decompressed sizes: common.lin={:#x}, map.lin={:#x}",
        common_lin_data.len(),
        map_lin_data.len(),
    );

    if args.no_checked {
        let mut lin_decoder = LinearFileDecoder::<LittleEndian, _>::new_unchecked(vec![
            Cursor::new(common_lin_data),
            Cursor::new(map_lin_data),
        ]);
        lin_decoder
            .decode_unchecked()
            .wrap_err("decode_unchecked failed")?;
        write_packages(&pkg_out, lin_decoder.linkers(), &lin_decoder.package_filenames())?;
        let stats = unrealin::diag::script_roundtrip_stats::<LittleEndian>(lin_decoder.linkers());
        stats.print_summary(20);
        // Dump all mismatches to a side file for grep-ability.
        if let Ok(mut f) = std::fs::File::create(output_dir.join("script_roundtrip_mismatches.txt")) {
            for m in &stats.mismatches {
                let _ = writeln!(
                    f,
                    "{} | {} | {} | captured={:#X} serialized={:#X} first_diff_at={:?}",
                    m.package, m.export_name, m.class_name,
                    m.captured_len, m.serialized_len, m.first_diff_at,
                );
            }
        }
    } else {
        let reader = BufReader::new(
            std::fs::File::open("reads.json").wrap_err("failed to open reads.json")?,
        );
        let mut metadata: ExportedData = serde_json::from_reader(reader)
            .wrap_err("failed to parse reads.json")?;
        metadata
            .file_reads
            .iter_mut()
            .for_each(|(_k, v)| v.reverse());

        let mut lin_decoder = LinearFileDecoder::<LittleEndian, _>::new_checked(
            vec![Cursor::new(common_lin_data), Cursor::new(map_lin_data)],
            metadata,
        );
        // The trace replay can still hit per-class divergence; catch the
        // panic so we still dump everything that was parsed before failure.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if let Err(e) = lin_decoder.decode_linear_file() {
                eprintln!("decode_linear_file err: {e}");
            }
        }));
        eprintln!(
            "trace ops consumed={} remaining={} ({:.1}%)",
            lin_decoder.trace_ops_consumed(),
            lin_decoder.trace_ops_remaining(),
            (lin_decoder.trace_ops_consumed() as f64
                / (lin_decoder.trace_ops_consumed() as f64
                    + lin_decoder.trace_ops_remaining() as f64).max(1.0))
                * 100.0,
        );
        write_packages(&pkg_out, lin_decoder.linkers(), &lin_decoder.package_filenames())?;
        let stats = unrealin::diag::script_roundtrip_stats::<LittleEndian>(lin_decoder.linkers());
        stats.print_summary(20);
    }

    Ok(())
}

fn write_packages(
    pkg_out: &std::path::Path,
    linkers: &std::collections::HashMap<String, std::rc::Rc<std::cell::RefCell<unrealin::de::Linker>>>,
    filenames: &std::collections::HashMap<String, String>,
) -> Result<()> {
    for (name, linker) in linkers {
        let linker = linker.borrow();
        // The file_table holds the package's original path relative to
        // the game's `System` dir (e.g. `Textures/HUD.utx`,
        // `Sounds/Dog.uax`). Joining onto `pkg_out` rebuilds the tree.
        let rel = filenames
            .get(&name.to_ascii_lowercase())
            .ok_or_else(|| eyre!("package {name:?} not in common.lin's file_table"))?;
        let path = pkg_out.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .wrap_err_with(|| format!("failed to create {parent:?}"))?;
        }
        let f = std::fs::File::create(&path)
            .wrap_err_with(|| format!("failed to create {path:?}"))?;
        let mut bw = BufWriter::new(f);
        if let Err(e) = unrealin::ser::serialize_linker_le(&linker, &mut bw) {
            eprintln!("warn: failed to serialize {name}: {e}");
        }
        bw.flush()?;
        println!(
            "wrote {path:?} ({} exports captured)",
            linker.captured.bodies.len()
        );
    }
    Ok(())
}
