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

    /// Decode a `.LS2` / `.SS2` codec_id=3 file (DARE IMA-ADPCM
    /// variant) end-to-end and write a `.wav`. The 28-byte header
    /// is parsed by `audio::codec3::parse_header`; ADPCM data
    /// starts at offset `0x48` and is decoded by the planar
    /// `sub_1983a0` kernel (mode=0, the dominant case) or the
    /// interleaved-stereo `sub_1984a0` kernel (mode=1, music).
    ///
    /// For `mode=0`, `track_count` mono ADPCM clips are
    /// concatenated head-to-tail in the file. The engine plays
    /// them in sequence as the mission progresses (track 0 first,
    /// then track 1, etc.). Output is one continuous mono WAV.
    DecodeLs2 {
        /// Input `.LS2` / `.SS2` file.
        input: PathBuf,
        /// Output `.wav` path.
        #[arg(short, long)]
        output: PathBuf,
        /// Override the sample rate. By default, the rate is
        /// hardcoded to 36000 Hz (matches the engine's
        /// `WAVEFORMATEX` builder at `sub_182b50`). Pass an
        /// explicit value to force a different rate for A/B
        /// testing.
        #[arg(long)]
        sample_rate: Option<u32>,
    },

    /// List the banks in a multi-bank `.SS2` container (e.g.
    /// `STREAM.SS2`). Each bank has a 0x2C-byte wrapper followed
    /// by an embedded codec_id=3 stream; the wrapper's `+0x08`
    /// field gives the bank size.
    Ss2List {
        /// Input multi-bank `.SS2` (e.g. `STREAM.SS2`).
        input: PathBuf,
    },

    /// Extract one or all embedded codec_id=3 streams from a
    /// multi-bank `.SS2` container into individual `.wav` files.
    Ss2Extract {
        /// Input multi-bank `.SS2` (e.g. `STREAM.SS2`).
        input: PathBuf,
        /// Output directory. Defaults to `<input_stem>_banks/`.
        /// Each bank lands at `<bank_index>.wav`.
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Extract a single bank by index instead of all.
        #[arg(long)]
        bank: Option<usize>,
        /// Override sample rate (default 36000 Hz, see DecodeLs2).
        #[arg(long)]
        sample_rate: Option<u32>,
    },

    /// Decode a codec_id=8 (proto/demo DARE-IMA) file. The kernel is
    /// a first-pass static port of the retail XBE's sub_1942e0;
    /// expect artifacts until trace validation is available.
    DecodeCodec8 {
        /// Input file (proto/demo `.SS2` / `.LS2` starting with
        /// `08 00 00 00`).
        input: PathBuf,
        /// Output `.wav` path.
        #[arg(short, long)]
        output: PathBuf,
        /// Override the header's sample rate.
        #[arg(long)]
        sample_rate: Option<u32>,
    },
}

#[derive(Parser, Debug)]
struct ExtractArgs {
    /// Where to extract files to. By default this will be the basename of the input file.
    /// For example, `common.lin` will extract to `common/`
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Replay a recorded I/O trace (a QEMU-plugin `reads.json`) instead
    /// of running the engine-faithful unchecked decoder. Trace mode
    /// exists as a development oracle — it asserts our reader's bytes
    /// match the engine's exact recorded ops, so it's useful for
    /// validating new stubs against ground truth. NOT for production
    /// extraction: traces are bound to the specific build they were
    /// recorded against (data-mismatched traces fail loudly).
    #[arg(long)]
    checked: Option<PathBuf>,

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

    /// Directory containing recorded I/O traces (`reads.json.<...>.bak`).
    /// Pairs are matched by session basename. Trace mode is a
    /// development oracle: it asserts our reader matches recorded
    /// engine I/O ops, useful for validating new stubs. Each trace is
    /// bound to the specific build it was recorded against; if the
    /// trace doesn't match the data (panics, panicked or under-
    /// consumes), the pair falls back to the unchecked decode.
    /// Default: no trace dir — every pair runs unchecked.
    #[arg(long)]
    trace_dir: Option<PathBuf>,
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
        AudioCmd::DecodeLs2 {
            input,
            output,
            sample_rate,
        } => run_decode_ls2(input, output, sample_rate),
        AudioCmd::Ss2List { input } => run_ss2_list(input),
        AudioCmd::Ss2Extract {
            input,
            output,
            bank,
            sample_rate,
        } => run_ss2_extract(input, output, bank, sample_rate),
        AudioCmd::DecodeCodec8 {
            input,
            output,
            sample_rate,
        } => run_decode_codec8(input, output, sample_rate),
    }
}

fn run_decode_codec8(
    input: PathBuf,
    output: PathBuf,
    sample_rate_override: Option<u32>,
) -> Result<()> {
    let bytes = std::fs::read(&input)
        .wrap_err_with(|| format!("failed to read {input:?}"))?;
    if !audio::codec8::is_codec8(&bytes) {
        return Err(eyre!(
            "{input:?}: not a codec_id=8 file (first byte != 0x08)"
        ));
    }
    let (header, pcm) = audio::codec8::decode_file(&bytes)
        .map_err(|e| eyre!("{input:?}: decode failed: {e}"))?;
    let channels = header.channels.max(1) as u16;
    let rate = sample_rate_override.unwrap_or(header.sample_rate);
    write_wav(&output, &pcm, channels, rate)?;
    eprintln!(
        "decoded {input:?}: rate={} ch={} samples={} -> {output:?}",
        rate,
        channels,
        pcm.len() / channels as usize
    );
    Ok(())
}

fn write_wav(path: &PathBuf, pcm: &[i16], channels: u16, sample_rate: u32) -> Result<()> {
    let spec = hound::WavSpec {
        channels,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec)
        .wrap_err_with(|| format!("failed to create {path:?}"))?;
    for s in pcm {
        writer.write_sample(*s)?;
    }
    writer.finalize()?;
    Ok(())
}

fn run_decode_ls2(
    input: PathBuf,
    output: PathBuf,
    sample_rate: Option<u32>,
) -> Result<()> {
    let file_bytes = std::fs::read(&input)
        .wrap_err_with(|| format!("failed to read {input:?}"))?;
    if audio::ss2::is_multibank(&file_bytes) {
        return Err(eyre!(
            "{input:?}: multi-bank container; use `audio ss2-list` / `ss2-extract` instead"
        ));
    }
    let header = audio::codec3::parse_header(&file_bytes)
        .map_err(|e| eyre!("{input:?}: {e}"))?;
    let pcm = audio::codec3::decode_file(&file_bytes)
        .map_err(|e| eyre!("{input:?}: decode failed: {e}"))?;

    let rate = sample_rate.unwrap_or_else(|| audio::codec3::header_sample_rate(&header));
    // mode=0: continuous mono (one ADPCM stream end-to-end).
    // mode=1: interleaved stereo (true stereo, music tracks).
    let channels: u16 = if header.mode == 1 { 2 } else { 1 };
    let frame_count = pcm.len() / channels as usize;

    write_wav(&output, &pcm, channels, rate)?;
    eprintln!(
        "decoded {input:?}: codec_id=3 v{} tracks={} mode={} block_period={:.4} -> {frame_count} frames @ {rate} Hz ({}ch) -> {output:?}",
        header.version, header.track_count, header.mode, header.block_period, channels,
    );
    Ok(())
}

fn run_ss2_list(input: PathBuf) -> Result<()> {
    let file_bytes = std::fs::read(&input)
        .wrap_err_with(|| format!("failed to read {input:?}"))?;
    if !audio::ss2::is_multibank(&file_bytes) {
        return Err(eyre!(
            "{input:?}: not a multi-bank container (first 8 bytes don't match `02 00 00 00 03 00 00 00`)"
        ));
    }
    let banks = audio::ss2::list(&file_bytes)
        .map_err(|e| eyre!("{input:?}: {e}"))?;
    println!("{}: {} bank(s); each bank has {} interleaved voices",
             input.display(), banks.len(), audio::ss2::VOICES_PER_BANK);
    println!("idx  offset      size       voice0_bp v0_tr v0_mode  voice1_bp v1_tr v1_mode  voice2_bp v2_tr v2_mode");
    for bank in &banks {
        print!("{:3}  {:#010x}  {:>10}", bank.index, bank.offset, bank.bank_size);
        for voice in 0..audio::ss2::VOICES_PER_BANK {
            let stream = bank.voice_stream(voice);
            match audio::codec3::parse_header(&stream) {
                Ok(h) => print!("  {:.5} {:>5} {:>5}",
                                h.block_period, h.track_count, h.mode),
                Err(_) => print!("  (parse failed)"),
            }
        }
        println!();
    }
    Ok(())
}

fn run_ss2_extract(
    input: PathBuf,
    output: Option<PathBuf>,
    bank_idx: Option<usize>,
    sample_rate: Option<u32>,
) -> Result<()> {
    let file_bytes = std::fs::read(&input)
        .wrap_err_with(|| format!("failed to read {input:?}"))?;
    if !audio::ss2::is_multibank(&file_bytes) {
        return Err(eyre!(
            "{input:?}: not a multi-bank container; use `audio decode-ls2` for single-stream files"
        ));
    }
    let banks = audio::ss2::list(&file_bytes)
        .map_err(|e| eyre!("{input:?}: {e}"))?;

    let out_dir = output.unwrap_or_else(|| {
        let stem = input.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "ss2".into());
        PathBuf::from(format!("{stem}_banks"))
    });
    std::fs::create_dir_all(&out_dir)
        .wrap_err_with(|| format!("failed to create {out_dir:?}"))?;

    let selected: Vec<&audio::ss2::Bank<'_>> = match bank_idx {
        Some(n) => {
            let bank = banks
                .get(n)
                .ok_or_else(|| eyre!("bank {n} out of range (file has {})", banks.len()))?;
            vec![bank]
        }
        None => banks.iter().collect(),
    };

    for bank in selected {
        for voice in 0..audio::ss2::VOICES_PER_BANK {
            let stream = bank.voice_stream(voice);
            let header = audio::codec3::parse_header(&stream)
                .map_err(|e| eyre!("bank {} voice {} header: {e}", bank.index, voice))?;
            let pcm = audio::codec3::decode_file(&stream)
                .map_err(|e| eyre!("bank {} voice {} decode: {e}", bank.index, voice))?;
            let rate = sample_rate.unwrap_or_else(|| audio::codec3::header_sample_rate(&header));
            let channels: u16 = if header.mode == 1 { 2 } else { 1 };
            let frame_count = pcm.len() / channels as usize;
            let out_path = out_dir.join(format!("{:03}_v{}.wav", bank.index, voice));
            write_wav(&out_path, &pcm, channels, rate)?;
            eprintln!(
                "bank {:3} voice {} @ {:#010x} ({} bytes ADPCM): mode={} tracks={} -> {frame_count} frames @ {rate} Hz ({}ch) -> {out_path:?}",
                bank.index, voice, bank.offset, stream.len(), header.mode, header.track_count, channels,
            );
        }
    }
    Ok(())
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
        "{:>5}  {:>8}  {:>14}  {:>10}  {:>5}  {:>3}  {:>4}  {}",
        "idx", "seq_id", "file_offset", "length", "rate", "ch", "ls2", "source_name"
    );
    for (i, e) in entries.iter().enumerate() {
        println!(
            "{:>5}  {:>#8x}  {:>#14x}  {:>#10x}  {:>5}  {:>3}  {:>4}  {}",
            i,
            e.seq_id,
            e.file_offset,
            e.length,
            e.sample_rate,
            e.channels,
            if e.is_ls2_redirect { "yes" } else { "" },
            e.source_name
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

    // For each source, capture both the bytes and the engine's logical
    // end-of-data: for `.lin` files that's the `uncompressed_data_size`
    // declared in metadata block 0 (decompressed buffers run a few
    // bytes past it as zlib alignment padding ending in `0xb3`); for
    // raw bin paths it's just the file length. Capping `LinReader` at
    // the declared size makes cross-source auto-advance land on the
    // next source's first byte without consuming the alignment tail
    // (009_ChineseEmbassy variant otherwise mis-shifts session.lin's
    // first PKG_TAG read).
    fn read_source(
        path: &std::path::Path,
        raw: &mut &[u8],
    ) -> color_eyre::Result<(Vec<u8>, u64)> {
        let is_lin = path
            .extension()
            .and_then(std::ffi::OsStr::to_str)
            .map(|s| s.eq_ignore_ascii_case("lin"))
            .unwrap_or(false);
        if is_lin {
            let (d, declared) =
                unrealin::de::decompress_linear_file_with_size::<LittleEndian, _>(raw)?;
            Ok((d, declared as u64))
        } else {
            let bytes = raw.to_vec();
            let len = bytes.len() as u64;
            Ok((bytes, len))
        }
    }

    let (common_lin_data, common_size) = read_source(&args.common_lin, &mut raw_common_file)?;
    let (map_lin_data, map_size) = read_source(&args.map_lin, &mut raw_map_file)?;

    let pkg_out = output_dir.join("packages");
    std::fs::create_dir_all(&pkg_out)?;

    let common_lin_data_len = common_lin_data.len();
    let map_lin_data_len = map_lin_data.len();
    eprintln!(
        "decompressed sizes: common.lin={:#x}, map.lin={:#x}",
        common_lin_data_len,
        map_lin_data_len,
    );

    if let Some(trace_path) = args.checked.as_ref() {
        let reader = BufReader::new(
            std::fs::File::open(trace_path).wrap_err_with(|| format!("failed to open {trace_path:?}"))?,
        );
        let mut metadata: ExportedData = serde_json::from_reader(reader)
            .wrap_err_with(|| format!("failed to parse {trace_path:?}"))?;
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
    } else {
        // Default: engine-faithful unchecked decode. Drives the
        // cascade through the same logic the engine's `LoadMap`
        // does (warmup classes, MyLevel cascade, post-MyLevel
        // PreBeginPlay/BeginPlay/PostBeginPlay/SetInitialState
        // bytecode walk). No recorded trace required.
        let mut lin_decoder = LinearFileDecoder::<LittleEndian, _>::new_unchecked(vec![
            unrealin::de::LinSource::new(Cursor::new(common_lin_data), common_size),
            unrealin::de::LinSource::new(Cursor::new(map_lin_data), map_size),
        ]);
        if let Err(e) = lin_decoder.decode_unchecked() {
            eprintln!("decode_unchecked partial (continuing with captured): {e}");
        }
        let consumed = lin_decoder.source_consumed_per_source();
        let caps = [common_size, map_size];
        for (i, c) in consumed.iter().enumerate() {
            let cap = caps[i];
            let pct = if cap == 0 { 0.0 } else { (*c as f64 / cap as f64) * 100.0 };
            eprintln!("source {i} consumed={c:#x}/{cap:#x} ({pct:.1}%)");
        }
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

    // No auto trace_dir: traces are an opt-in development oracle
    // (see ExtractArgs::checked). Pass `--trace-dir <dir>` explicitly
    // to use one; default merges every pair via the engine-faithful
    // unchecked decoder.
    let report = merge::run_merge(&args.game_dir, &output_dir, args.trace_dir.as_deref())?;
    report.print_summary();
    Ok(())
}
