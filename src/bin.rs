use std::{
    io::{BufReader, Cursor, Write},
    path::PathBuf,
};

use byteorder::LittleEndian;
use clap::{Parser, Subcommand};
use color_eyre::{
    Result,
    eyre::{Context, eyre},
};
use tracing_subscriber::{EnvFilter, fmt};
use unrealin::{
    ExportedData,
    audio::{self, ImaHeader},
    de::LinearFileDecoder,
    merge,
};

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Extract one `(common.lin, map.lin)` pair into per-package files.
    Extract(ExtractArgs),

    /// Walk a game directory's `LMaps/`, parse every `(common.lin, session.lin)`
    /// pair, and union the partial captures into a single per-package tree.
    Merge(MergeArgs),

    /// Inspect or decode a Splinter Cell 1 Xbox audio file (.SS2 / .LS2).
    /// Format is the DARE IMA-ADPCM v3 variant; see docs/AUDIO.md for the
    /// known fields and the still-open questions.
    Audio {
        #[command(subcommand)]
        cmd: AudioCmd,
    },
}

#[derive(Subcommand, Debug)]
enum AudioCmd {
    /// Print the 28-byte DARE IMA-ADPCM v3 header for an SS2/LS2 file.
    Info { input: PathBuf },

    /// Decode an SS2/LS2 single-stream file to PCM. Writes WAV by
    /// default; pass `--raw` for headerless little-endian 16-bit PCM.
    /// The decoder is best-effort and may produce noise until the
    /// chunk-prologue cadence is fully reverse-engineered.
    Decode {
        input: PathBuf,

        /// Output path. Defaults to `<input>.wav` (or `.pcm` with --raw).
        #[arg(short, long)]
        output: Option<PathBuf>,

        /// Write raw little-endian 16-bit interleaved PCM instead of WAV.
        #[arg(long)]
        raw: bool,

        /// Override the sample rate the decoder writes into the WAV
        /// header (default: 22050). The actual rate field in the SS2
        /// header is not yet decoded.
        #[arg(long, default_value_t = 22050)]
        sample_rate: u32,
    },
}

#[derive(Parser, Debug)]
struct ExtractArgs {
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

#[derive(Parser, Debug)]
struct MergeArgs {
    /// Where merged packages go. Defaults to `<game_dir>/merged/`.
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Game root containing the `LMaps/` directory.
    game_dir: PathBuf,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    let subscriber = fmt().pretty().with_env_filter(filter).finish();
    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");

    match cli.cmd {
        Cmd::Extract(args) => run_extract(args),
        Cmd::Merge(args) => run_merge_cmd(args),
        Cmd::Audio { cmd } => run_audio(cmd),
    }
}

fn run_audio(cmd: AudioCmd) -> Result<()> {
    match cmd {
        AudioCmd::Info { input } => run_audio_info(input),
        AudioCmd::Decode {
            input,
            output,
            raw,
            sample_rate,
        } => run_audio_decode(input, output, raw, sample_rate),
    }
}

fn run_audio_info(input: PathBuf) -> Result<()> {
    let bytes = std::fs::read(&input)
        .wrap_err_with(|| format!("failed to read {:?}", &input))?;
    let header = ImaHeader::parse(&bytes)
        .wrap_err_with(|| format!("failed to parse header for {:?}", &input))?;

    println!("file        : {}", input.display());
    println!("size        : {} bytes", bytes.len());
    println!("codec ver   : 0x{:02x} (v3)", header.codec_version);
    println!("bytes 1..3  : {:02x?}", header.raw_bytes_1_3);
    println!("ratio (f32) : {}", header.ratio);
    println!("bytes 8..11 : {:02x?}", header.raw_8_11);
    println!(
        "stereo flag : 0x{:02x} (channels = {})",
        header.stereo_flag,
        header.channels()
    );
    println!("byte 13     : 0x{:02x}", header.raw_byte_13);
    println!(
        "field 14 BE : 0x{:04x} ({} dec)",
        header.field_14, header.field_14
    );
    println!(
        "field 16 BE : 0x{:04x} ({} dec)",
        header.field_16, header.field_16
    );
    println!("bytes 18..19: {:02x?}", header.raw_18_19);
    println!(
        "field 20 BE : 0x{:04x} ({} dec)",
        header.field_20, header.field_20
    );
    println!("bytes 22..25: {:02x?}", header.raw_22_25);
    println!(
        "field 26 BE : 0x{:04x} ({} dec)",
        header.field_26, header.field_26
    );
    println!();
    println!("derived (best guess):");
    println!("  samples_per_block : {}", header.samples_per_block());
    println!("  bits_per_sample   : {}", header.bits_per_sample());
    println!(
        "  block_bytes/ch    : {}",
        header.block_bytes_per_channel()
    );
    println!(
        "  sample_rate guess : {} Hz (override with --sample-rate on decode)",
        header.sample_rate_guess()
    );
    Ok(())
}

fn run_audio_decode(
    input: PathBuf,
    output: Option<PathBuf>,
    raw: bool,
    sample_rate: u32,
) -> Result<()> {
    let bytes = std::fs::read(&input)
        .wrap_err_with(|| format!("failed to read {:?}", &input))?;
    let decoded = audio::decode_single_stream(&bytes)
        .wrap_err_with(|| format!("failed to decode {:?}", &input))?;

    let out_path = output.unwrap_or_else(|| {
        let mut p = input.clone();
        p.set_extension(if raw { "pcm" } else { "wav" });
        p
    });

    eprintln!(
        "decoded {} samples ({} channels) to {:?}",
        decoded.pcm.len() / decoded.header.channels() as usize,
        decoded.header.channels(),
        out_path,
    );

    if raw {
        let mut out = std::fs::File::create(&out_path)
            .wrap_err_with(|| format!("failed to create {:?}", &out_path))?;
        let mut buf = Vec::with_capacity(decoded.pcm.len() * 2);
        for s in &decoded.pcm {
            buf.extend_from_slice(&s.to_le_bytes());
        }
        out.write_all(&buf)?;
    } else {
        let spec = hound::WavSpec {
            channels: decoded.header.channels(),
            sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&out_path, spec)
            .wrap_err_with(|| format!("failed to create wav {:?}", &out_path))?;
        for s in &decoded.pcm {
            writer.write_sample(*s)?;
        }
        writer.finalize()?;
    }

    Ok(())
}

fn run_extract(mut args: ExtractArgs) -> Result<()> {
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
        merge::write_packages(&pkg_out, lin_decoder.linkers(), &lin_decoder.package_filenames())?;
        let stats = unrealin::diag::script_roundtrip_stats::<LittleEndian>(lin_decoder.linkers());
        stats.print_summary(20);
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
        merge::write_packages(&pkg_out, lin_decoder.linkers(), &lin_decoder.package_filenames())?;
        let stats = unrealin::diag::script_roundtrip_stats::<LittleEndian>(lin_decoder.linkers());
        stats.print_summary(20);
    }

    Ok(())
}

fn run_merge_cmd(args: MergeArgs) -> Result<()> {
    let output_dir = args
        .output
        .clone()
        .unwrap_or_else(|| args.game_dir.join("merged"));
    std::fs::create_dir_all(&output_dir)
        .wrap_err_with(|| format!("failed to create output dir {output_dir:?}"))?;

    let report = merge::run_merge(&args.game_dir, &output_dir)?;
    report.print_summary();
    Ok(())
}
