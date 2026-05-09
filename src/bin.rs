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
use unrealin::{ExportedData, audio, de::LinearFileDecoder, merge};

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

    /// Decode Splinter Cell 1 Xbox audio. The on-disk format inside
    /// `.SS2` / `.LS2` / `.uax` / `.SM2` containers is raw Xbox-ADPCM
    /// (`wFormatTag = 0x0069`), confirmed via QEMU plugin trace of
    /// `IDirectSoundBuffer::SetBufferData`; see docs/AUDIO.md.
    Audio {
        #[command(subcommand)]
        cmd: AudioCmd,
    },
}

#[derive(Subcommand, Debug)]
enum AudioCmd {
    /// List the outer directory of a `.SM2` (Maps BigFile). Each
    /// record names a map and gives the (offset, size) of its
    /// per-map descriptor blob within the file. Format reversed from
    /// `sub_17e250` / `sub_17e3f0`.
    Sm2List { input: PathBuf },

    /// Extract a single map's descriptor blob from a `.SM2` file.
    /// The blob itself is a fixup-relative pointer-graph (see
    /// `sub_17e470`).
    Sm2ExtractMap {
        input: PathBuf,
        /// Map name as listed by `sm2-list` (e.g. `0_0_2_Training`).
        #[arg(long)]
        name: String,
        /// Output path. Defaults to `<map_name>.sm2map`.
        #[arg(short, long)]
        output: Option<PathBuf>,
    },

    /// List every sound entry inside one map of a `.SM2` file.
    /// Each entry is `(seq_id, file_offset, length)`; the audio is
    /// raw Xbox-IMA-ADPCM at that offset.
    Sm2ListSounds {
        input: PathBuf,
        /// Map name as listed by `sm2-list`.
        #[arg(long)]
        map: String,
    },

    /// Decode every sound in one map of a `.SM2` file into a
    /// directory of `.wav` files. Each sound's sample rate, channel
    /// count, and source `.wav` name come from the descriptor
    /// metadata at `array_b[seq_id]`.
    Sm2ExtractSounds {
        input: PathBuf,
        /// Map name as listed by `sm2-list`.
        #[arg(long)]
        map: String,
        /// Output directory. Defaults to `<map_name>_sounds/`.
        #[arg(short, long)]
        output: Option<PathBuf>,
    },

    /// Decode every sound from every map in a `.SM2` file into one
    /// output directory tree (one subdir per map).
    Sm2ExtractAll {
        input: PathBuf,
        /// Output directory. Defaults to `<input_stem>_sounds/`.
        #[arg(short, long)]
        output: Option<PathBuf>,
    },

    /// Decode a raw Xbox-ADPCM byte range from a file at a known
    /// offset and length. Useful for verifying the decoder against a
    /// `SetBufferData` trace capture before container parsing exists.
    DecodeRegion {
        /// File to read from (typically a `.SM2` / `.SS2` / `.LS2`
        /// / `.uax`).
        input: PathBuf,

        /// Byte offset of the audio payload within the file.
        #[arg(long)]
        offset: u64,

        /// Length of the audio payload in bytes. Must be a multiple
        /// of 36 (mono) or 72 (stereo).
        #[arg(long)]
        length: usize,

        /// Channel count. Mono = 36 bytes/block, stereo = 72.
        #[arg(long, default_value_t = 1)]
        channels: u16,

        /// Sample rate to write into the WAV header.
        #[arg(long, default_value_t = 44100)]
        sample_rate: u32,

        /// Output path (default: `<input>_region.wav`).
        #[arg(short, long)]
        output: Option<PathBuf>,

        /// Emit headerless little-endian 16-bit PCM instead of WAV.
        #[arg(long)]
        raw: bool,
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
        AudioCmd::Sm2List { input } => run_sm2_list(input),
        AudioCmd::Sm2ExtractMap { input, name, output } => run_sm2_extract(input, name, output),
        AudioCmd::Sm2ListSounds { input, map } => run_sm2_list_sounds(input, map),
        AudioCmd::Sm2ExtractSounds {
            input,
            map,
            output,
        } => run_sm2_extract_sounds(input, map, output),
        AudioCmd::Sm2ExtractAll { input, output } => run_sm2_extract_all(input, output),
        AudioCmd::DecodeRegion {
            input,
            offset,
            length,
            channels,
            sample_rate,
            output,
            raw,
        } => run_audio_decode_region(
            input, offset, length, channels, sample_rate, output, raw,
        ),
    }
}

fn load_map_descriptor(input: &PathBuf, map_name: &str) -> Result<(audio::sm2::Record, Option<u32>, Vec<u8>)> {
    let mut file = std::fs::File::open(input)
        .wrap_err_with(|| format!("failed to open {input:?}"))?;
    let records = audio::sm2::read_directory(&mut file)
        .wrap_err_with(|| format!("failed to parse {input:?} as .SM2"))?;
    let idx = records
        .iter()
        .position(|r| r.name == map_name)
        .ok_or_else(|| eyre!("no record named {map_name:?} in {input:?}"))?;
    let rec = records[idx].clone();
    let next_off = records.get(idx + 1).map(|r| r.data_offset);

    use std::io::{Read as _, Seek as _, SeekFrom};
    file.seek(SeekFrom::Start(rec.data_offset as u64))?;
    let mut buf = vec![0u8; rec.data_size as usize];
    file.read_exact(&mut buf)
        .wrap_err_with(|| format!("failed to read descriptor at {:#x}", rec.data_offset))?;
    Ok((rec, next_off, buf))
}

fn run_sm2_extract_all(input: PathBuf, output: Option<PathBuf>) -> Result<()> {
    let mut file = std::fs::File::open(&input)
        .wrap_err_with(|| format!("failed to open {input:?}"))?;
    let records = audio::sm2::read_directory(&mut file)
        .wrap_err_with(|| format!("failed to parse {input:?} as .SM2"))?;
    drop(file);

    let stem = input
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("audio");
    let root = output.unwrap_or_else(|| PathBuf::from(format!("{stem}_sounds")));
    std::fs::create_dir_all(&root)
        .wrap_err_with(|| format!("failed to create {root:?}"))?;

    let total_maps = records.len();
    let mut total_ok = 0usize;
    let mut total_skipped = 0usize;
    for (i, rec) in records.iter().enumerate() {
        let sub = root.join(&rec.name);
        eprintln!(
            "[{}/{}] {} ({} bytes desc @ {:#x})",
            i + 1,
            total_maps,
            rec.name,
            rec.data_size,
            rec.data_offset
        );
        match run_sm2_extract_sounds(input.clone(), rec.name.clone(), Some(sub)) {
            Ok(()) => total_ok += 1,
            Err(e) => {
                eprintln!("  failed: {e:#}");
                total_skipped += 1;
            }
        }
    }
    eprintln!(
        "\ndone: {} maps OK, {} maps failed -> {root:?}",
        total_ok, total_skipped
    );
    Ok(())
}

fn run_sm2_list_sounds(input: PathBuf, map: String) -> Result<()> {
    let (rec, next_off, desc) = load_map_descriptor(&input, &map)?;
    let entries = audio::sm2::parse_sound_table(&desc, &rec, next_off)
        .wrap_err("failed to parse sound table")?;
    println!("{} sounds in map {:?}:", entries.len(), map);
    println!(
        "{:>5}  {:>8}  {:>14}  {:>10}  {:>5}  {:>3}  {}",
        "idx", "seq_id", "file_offset", "length", "rate", "ch", "source_name"
    );
    for (i, e) in entries.iter().enumerate() {
        println!(
            "{:>5}  {:>#8x}  {:>#14x}  {:>#10x}  {:>5}  {:>3}  {}",
            i, e.seq_id, e.file_offset, e.length, e.sample_rate, e.channels, e.source_name
        );
    }
    Ok(())
}

fn run_sm2_extract_sounds(
    input: PathBuf,
    map: String,
    output: Option<PathBuf>,
) -> Result<()> {
    let (rec, next_off, desc) = load_map_descriptor(&input, &map)?;
    let entries = audio::sm2::parse_sound_table(&desc, &rec, next_off)
        .wrap_err("failed to parse sound table")?;

    let out_dir = output.unwrap_or_else(|| PathBuf::from(format!("{}_sounds", map)));
    std::fs::create_dir_all(&out_dir)
        .wrap_err_with(|| format!("failed to create {out_dir:?}"))?;

    let mut file = std::fs::File::open(&input)
        .wrap_err_with(|| format!("failed to open {input:?}"))?;
    use std::io::{Read as _, Seek as _, SeekFrom};

    let mut ok = 0usize;
    let mut skipped = 0usize;
    for (i, e) in entries.iter().enumerate() {
        if e.length == 0 {
            skipped += 1;
            continue;
        }
        let block_size = audio::BLOCK_BYTES_PER_CHANNEL * e.channels as usize;
        let len = e.length as usize;
        if len % block_size != 0 {
            eprintln!(
                "skipping idx={i} seq={:#x}: length {:#x} not multiple of block size {block_size}",
                e.seq_id, e.length
            );
            skipped += 1;
            continue;
        }
        file.seek(SeekFrom::Start(e.file_offset))?;
        let mut buf = vec![0u8; len];
        file.read_exact(&mut buf).wrap_err_with(|| {
            format!(
                "failed to read {len} bytes at {:#x} for idx={i}",
                e.file_offset
            )
        })?;
        let pcm = audio::decode(&buf, e.channels);

        // Use source .wav name when available, otherwise fall back to seq_id.
        let stem = if e.source_name.is_empty() {
            format!("{:03}_seq{:04x}", i, e.seq_id)
        } else {
            // Strip `.wav` if the metadata name includes it.
            let n = e.source_name.trim_end_matches(".wav");
            format!("{:03}_{}", i, n)
        };
        let out_path = out_dir.join(format!("{stem}.wav"));

        let spec = hound::WavSpec {
            channels: e.channels,
            sample_rate: e.sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&out_path, spec)
            .wrap_err_with(|| format!("failed to create {out_path:?}"))?;
        for s in &pcm {
            writer.write_sample(*s)?;
        }
        writer.finalize()?;
        ok += 1;
    }
    eprintln!(
        "extracted {} sounds to {:?} ({} skipped)",
        ok, out_dir, skipped
    );
    Ok(())
}

fn run_sm2_list(input: PathBuf) -> Result<()> {
    let mut file = std::fs::File::open(&input)
        .wrap_err_with(|| format!("failed to open {input:?}"))?;
    let records = audio::sm2::read_directory(&mut file)
        .wrap_err_with(|| format!("failed to parse {input:?} as .SM2"))?;
    println!("{:<35} {:>14} {:>14}", "name", "data_offset", "data_size");
    for r in &records {
        println!(
            "{:<35} {:>#14x} {:>#14x}  ({} B)",
            r.name, r.data_offset, r.data_size, r.data_size
        );
    }
    println!("\n{} records.", records.len());
    Ok(())
}

fn run_sm2_extract(input: PathBuf, name: String, output: Option<PathBuf>) -> Result<()> {
    let mut file = std::fs::File::open(&input)
        .wrap_err_with(|| format!("failed to open {input:?}"))?;
    let records = audio::sm2::read_directory(&mut file)
        .wrap_err_with(|| format!("failed to parse {input:?} as .SM2"))?;
    let rec = records
        .iter()
        .find(|r| r.name == name)
        .ok_or_else(|| eyre!("no record named {name:?} in {input:?}"))?;

    use std::io::{Read as _, Seek as _, SeekFrom};
    file.seek(SeekFrom::Start(rec.data_offset as u64))?;
    let mut buf = vec![0u8; rec.data_size as usize];
    file.read_exact(&mut buf)
        .wrap_err_with(|| format!("failed to read {} bytes at {:#x}", rec.data_size, rec.data_offset))?;

    let out_path = output.unwrap_or_else(|| PathBuf::from(format!("{}.sm2map", name)));
    std::fs::write(&out_path, &buf)
        .wrap_err_with(|| format!("failed to write {out_path:?}"))?;
    eprintln!(
        "extracted {name:?}: {} bytes from {:#x} -> {out_path:?}",
        rec.data_size, rec.data_offset
    );
    Ok(())
}

fn run_audio_decode_region(
    input: PathBuf,
    offset: u64,
    length: usize,
    channels: u16,
    sample_rate: u32,
    output: Option<PathBuf>,
    raw: bool,
) -> Result<()> {
    if !matches!(channels, 1 | 2) {
        return Err(eyre!("--channels must be 1 or 2, got {channels}"));
    }
    let block_size = audio::BLOCK_BYTES_PER_CHANNEL * channels as usize;
    if length % block_size != 0 {
        return Err(eyre!(
            "--length {length} is not a multiple of block size {block_size} for {channels}-channel"
        ));
    }

    let mut file = std::fs::File::open(&input)
        .wrap_err_with(|| format!("failed to open {input:?}"))?;
    use std::io::{Read as _, Seek as _, SeekFrom};
    file.seek(SeekFrom::Start(offset))
        .wrap_err_with(|| format!("failed to seek to {offset:#x} in {input:?}"))?;
    let mut buf = vec![0u8; length];
    file.read_exact(&mut buf)
        .wrap_err_with(|| format!("failed to read {length} bytes from {input:?}"))?;

    let pcm = audio::decode(&buf, channels);
    eprintln!(
        "decoded {} samples ({} channels, {} blocks)",
        pcm.len() / channels as usize,
        channels,
        length / block_size,
    );

    let out_path = output.unwrap_or_else(|| {
        let stem = input
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("audio");
        let ext = if raw { "pcm" } else { "wav" };
        input.with_file_name(format!("{stem}_region.{ext}"))
    });

    if raw {
        let mut out = std::fs::File::create(&out_path)
            .wrap_err_with(|| format!("failed to create {out_path:?}"))?;
        let mut bytes = Vec::with_capacity(pcm.len() * 2);
        for s in &pcm {
            bytes.extend_from_slice(&s.to_le_bytes());
        }
        out.write_all(&bytes)?;
    } else {
        let spec = hound::WavSpec {
            channels,
            sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&out_path, spec)
            .wrap_err_with(|| format!("failed to create wav {out_path:?}"))?;
        for s in &pcm {
            writer.write_sample(*s)?;
        }
        writer.finalize()?;
    }
    eprintln!("wrote {out_path:?}");
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
