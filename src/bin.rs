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
    /// Walk an audio file (or directory of audio files) and dump
    /// every container's banks/sounds as individual `.wav` files.
    /// Codec is auto-detected per file from the header:
    ///   - first byte `0x08` → proto LS2/SS2 (codec_id=8). Each
    ///     bank → `bank_NN.wav` under `<output>/<rel_stem>/`.
    ///   - first byte `0x03` → retail single-stream LS2 (codec_id=3).
    ///     One mono or stereo `.wav` under `<output>/<rel_stem>/`.
    ///   - `02 00 00 00 03 00 00 00` → multi-bank SS2 (codec_id=3
    ///     container, e.g. `STREAM.SS2`). Each bank's three voices →
    ///     `bank_NN_voice_M.wav`.
    ///   - first dword `0x07` → `.SM2` Maps BigFile. Each map →
    ///     subdir of per-sound wavs.
    ///   - Other codecs → skipped (logged).
    /// Output structure mirrors retail's per-map convention.
    Dump {
        /// Input file or directory.
        input: PathBuf,
        /// Output directory root. Defaults to `<input_basename>_wav`.
        #[arg(short, long)]
        output: Option<PathBuf>,
    },

    /// Decode a raw Xbox-ADPCM byte range from a file at a known
    /// offset and length. Useful for verifying the decoder against
    /// a `SetBufferData` trace capture before container parsing
    /// exists, and for debugging odd ad-hoc clips.
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
        AudioCmd::Dump { input, output } => run_dump(input, output),
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

fn run_dump(input: PathBuf, output: Option<PathBuf>) -> Result<()> {
    let metadata = std::fs::metadata(&input)
        .wrap_err_with(|| format!("failed to stat {input:?}"))?;
    let root = output.unwrap_or_else(|| {
        let stem = input
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("audio");
        PathBuf::from(format!("{stem}_wav"))
    });
    std::fs::create_dir_all(&root)
        .wrap_err_with(|| format!("failed to create {root:?}"))?;
    let (base, files): (PathBuf, Vec<PathBuf>) = if metadata.is_dir() {
        (input.clone(), walk_audio_files(&input)?)
    } else {
        (
            input.parent().unwrap_or_else(|| std::path::Path::new(".")).to_path_buf(),
            vec![input.clone()],
        )
    };
    let total = files.len();
    let mut ok = 0usize;
    let mut skipped = 0usize;
    for (i, src) in files.iter().enumerate() {
        let rel = src.strip_prefix(&base).unwrap_or(src);
        let stem = rel.with_extension("");
        let dst_dir = root.join(&stem);
        eprintln!("[{}/{}] {}", i + 1, total, rel.display());
        match dump_file_banks(src, &dst_dir) {
            Ok(n) => {
                eprintln!("  -> {n} bank(s) -> {dst_dir:?}");
                ok += 1;
            }
            Err(e) => {
                eprintln!("  skipped: {e:#}");
                skipped += 1;
            }
        }
    }
    eprintln!("\ndone: {} files OK, {} skipped -> {root:?}", ok, skipped);
    Ok(())
}

fn walk_audio_files(root: &std::path::Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)
            .wrap_err_with(|| format!("failed to read {dir:?}"))?
        {
            let entry = entry?;
            let path = entry.path();
            let ft = entry.file_type()?;
            if ft.is_dir() {
                stack.push(path);
            } else if ft.is_file() {
                if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
                    let ext_u = ext.to_ascii_uppercase();
                    if matches!(ext_u.as_str(), "LS2" | "SS2" | "UAX" | "SM2" | "LM2") {
                        out.push(path);
                    }
                }
            }
        }
    }
    out.sort();
    Ok(out)
}

/// Dump every bank/sound of one audio file into `dst_dir` as
/// `bank_NN.wav` (or per-map subdirs for SM2). Returns the number
/// of WAVs written. Dispatches by the file's header:
///   - first byte `0x08`            → codec_id=8 (proto LS2/SS2)
///   - first byte `0x03`            → codec_id=3 (retail LS2 single)
///   - `02 00 00 00 03 00 00 00`    → multi-bank SS2 (codec_id=3)
///   - first dword `0x07`           → SM2 Maps BigFile
fn dump_file_banks(src: &std::path::Path, dst_dir: &std::path::Path) -> Result<usize> {
    let bytes = std::fs::read(src)
        .wrap_err_with(|| format!("failed to read {src:?}"))?;
    if bytes.is_empty() {
        return Err(eyre!("empty file"));
    }
    if audio::ss2::is_multibank(&bytes) {
        return dump_ss2_multibank(&bytes, dst_dir);
    }
    if bytes.len() >= 4
        && u32::from_le_bytes(bytes[0..4].try_into().unwrap()) == audio::sm2::VERSION
    {
        return dump_sm2(src, dst_dir);
    }
    match bytes[0] {
        0x08 => dump_codec8(&bytes, dst_dir),
        0x03 => dump_codec3_single(&bytes, dst_dir),
        b => Err(eyre!("unsupported codec id 0x{b:02x}")),
    }
}

fn dump_codec8(bytes: &[u8], dst_dir: &std::path::Path) -> Result<usize> {
    use std::fmt::Write as _;
    let banks = audio::codec8::decode_each_bank(bytes)
        .map_err(|e| eyre!("decode failed: {e}"))?;
    if banks.is_empty() {
        return Err(eyre!("no banks found"));
    }
    std::fs::create_dir_all(dst_dir)
        .wrap_err_with(|| format!("failed to create {dst_dir:?}"))?;
    let channels = if banks[0].header.unknown_2c == 2 { 2u16 } else { 1u16 };
    let rate = banks[0].header.sample_rate;
    let subtype = banks[0].header.unknown_2c;
    let mut all = Vec::new();
    let mut bank_files = Vec::with_capacity(banks.len());
    for (i, bank) in banks.iter().enumerate() {
        let filename = format!("bank_{:02}.wav", i);
        write_wav(&dst_dir.join(&filename), &bank.pcm, channels, rate)?;
        all.extend_from_slice(&bank.pcm);
        bank_files.push(filename);
    }
    if banks.len() > 1 {
        write_wav(&dst_dir.join("all_banks.wav"), &all, channels, rate)?;
    }
    let mut toc = String::new();
    writeln!(
        toc,
        "codec = 8\nsubtype = {subtype}\nsample_rate = {rate}\nchannels = {channels}\n"
    )
    .ok();
    toc.push_str("banks = [\n");
    for f in &bank_files {
        writeln!(toc, "    {f:?},").ok();
    }
    toc.push_str("]\n");
    std::fs::write(dst_dir.join("toc.toml"), toc)
        .wrap_err_with(|| format!("write toc.toml in {dst_dir:?}"))?;
    Ok(banks.len())
}

fn dump_codec3_single(bytes: &[u8], dst_dir: &std::path::Path) -> Result<usize> {
    // codec_id=3 LS2/SS2 files have no per-clip byte boundaries
    // recoverable from the file alone. `track_count` is engine-script
    // metadata (cue counts), not a byte-layout indicator — the
    // payload is one continuous IMA-ADPCM stream where clip
    // boundaries land at silence and aren't equally spaced. Dump as a
    // single bank; the individually-named clips are already in
    // `<lang>/MAPS/<map>/<name>.wav` from LM2's sound table.
    let header = audio::codec3::parse_header(bytes)
        .map_err(|e| eyre!("parse_header: {e}"))?;
    let pcm = audio::codec3::decode_file(bytes)
        .map_err(|e| eyre!("decode_file: {e}"))?;
    std::fs::create_dir_all(dst_dir)
        .wrap_err_with(|| format!("failed to create {dst_dir:?}"))?;
    let channels = if header.mode == 1 { 2u16 } else { 1u16 };
    let rate = audio::codec3::header_sample_rate(&header);
    write_wav(&dst_dir.join("bank_00.wav"), &pcm, channels, rate)?;
    let toc = format!(
        "codec = 3\nmode = {}\ntrack_count = {}\nsample_rate = {rate}\nchannels = {channels}\n\nbanks = [\n    \"bank_00.wav\",\n]\n",
        header.mode, header.track_count,
    );
    std::fs::write(dst_dir.join("toc.toml"), toc)
        .wrap_err_with(|| format!("write toc.toml in {dst_dir:?}"))?;
    Ok(1)
}

fn dump_ss2_multibank(bytes: &[u8], dst_dir: &std::path::Path) -> Result<usize> {
    use std::fmt::Write as _;
    let banks = audio::ss2::list(bytes)
        .map_err(|e| eyre!("ss2::list: {e}"))?;
    if banks.is_empty() {
        return Err(eyre!("no banks found"));
    }
    std::fs::create_dir_all(dst_dir)
        .wrap_err_with(|| format!("failed to create {dst_dir:?}"))?;
    let mut toc = String::new();
    writeln!(
        toc,
        "codec = 3\ncontainer = \"ss2-multibank\"\nvoices_per_bank = {}\n",
        audio::ss2::VOICES_PER_BANK
    )
    .ok();
    toc.push_str("banks = [\n");
    let mut written = 0usize;
    // Accumulate per-voice PCM across banks so the per-voice
    // `all_voice_N.wav` matches proto's `all_banks.wav` continuous-
    // stream concept. Each SS2 voice is one logical audio track that
    // the engine plays in sequence across banks (the wrapper splits
    // the stream for streaming buffering, not for playback semantics).
    let mut all_voices: Vec<Vec<i16>> =
        vec![Vec::new(); audio::ss2::VOICES_PER_BANK];
    let mut voice_channels = [1u16; audio::ss2::VOICES_PER_BANK];
    let mut voice_rates = [0u32; audio::ss2::VOICES_PER_BANK];
    for bank in &banks {
        toc.push_str("    [\n");
        for voice in 0..audio::ss2::VOICES_PER_BANK {
            let voice_stream = bank.voice_stream(voice);
            let header = audio::codec3::parse_header(&voice_stream)
                .map_err(|e| eyre!("bank {} voice {} header: {e}", bank.index, voice))?;
            let pcm = audio::ss2::decode_voice(bank, voice)
                .map_err(|e| eyre!("bank {} voice {} decode: {e}", bank.index, voice))?;
            let channels = if header.mode == 1 { 2u16 } else { 1u16 };
            let rate = audio::codec3::header_sample_rate(&header);
            voice_channels[voice] = channels;
            voice_rates[voice] = rate;
            let filename = format!("bank_{:02}_voice_{}.wav", bank.index, voice);
            write_wav(&dst_dir.join(&filename), &pcm, channels, rate)?;
            all_voices[voice].extend_from_slice(&pcm);
            writeln!(toc, "        {filename:?},").ok();
            written += 1;
        }
        toc.push_str("    ],\n");
    }
    toc.push_str("]\n");
    if banks.len() > 1 {
        for (voice, pcm) in all_voices.iter().enumerate() {
            let filename = format!("all_voice_{}.wav", voice);
            write_wav(
                &dst_dir.join(&filename),
                pcm,
                voice_channels[voice],
                voice_rates[voice],
            )?;
        }
    }
    std::fs::write(dst_dir.join("toc.toml"), toc)
        .wrap_err_with(|| format!("write toc.toml in {dst_dir:?}"))?;
    Ok(written)
}

fn dump_sm2(src: &std::path::Path, dst_dir: &std::path::Path) -> Result<usize> {
    use std::fmt::Write as _;
    use std::io::{Read, Seek, SeekFrom};
    let kind = match src
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_uppercase())
        .as_deref()
    {
        Some("LM2") => audio::sm2::Kind::Lm2,
        _ => audio::sm2::Kind::Sm2,
    };
    let kind_str = match kind {
        audio::sm2::Kind::Lm2 => "lm2",
        audio::sm2::Kind::Sm2 => "sm2",
    };
    let mut file = std::fs::File::open(src)
        .wrap_err_with(|| format!("failed to open {src:?}"))?;
    let records = audio::sm2::read_directory(&mut file)
        .wrap_err("sm2::read_directory")?;
    std::fs::create_dir_all(dst_dir)
        .wrap_err_with(|| format!("failed to create {dst_dir:?}"))?;
    let mut toc = String::new();
    writeln!(toc, "kind = {kind_str:?}").ok();
    let mut written = 0usize;
    for (idx, rec) in records.iter().enumerate() {
        let map_dir = dst_dir.join(&rec.name);
        let next_off = records.get(idx + 1).map(|r| r.data_offset);
        file.seek(SeekFrom::Start(rec.data_offset as u64))?;
        let mut desc = vec![0u8; rec.data_size as usize];
        file.read_exact(&mut desc)
            .wrap_err_with(|| format!("read map {} descriptor", rec.name))?;
        let entries = match audio::sm2::parse_sound_table_for(&desc, rec, next_off, kind) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("    {}: parse_sound_table failed: {e}", rec.name);
                continue;
            }
        };
        if entries.is_empty() {
            continue;
        }
        std::fs::create_dir_all(&map_dir)
            .wrap_err_with(|| format!("failed to create {map_dir:?}"))?;
        writeln!(toc, "\n[[maps]]\nname = {:?}\nsounds = [", rec.name).ok();
        // Track names used so duplicates get a `_dupN` suffix instead
        // of silently overwriting each other.
        let mut seen: std::collections::HashMap<String, u32> = Default::default();
        for e in entries.iter() {
            let base = if e.source_name.is_empty() {
                format!("seq{:04x}", e.seq_id)
            } else {
                e.source_name.trim_end_matches(".wav").to_string()
            };
            let dup = seen.entry(base.clone()).or_insert(0);
            let filename = if *dup == 0 {
                format!("{base}.wav")
            } else {
                format!("{base}_dup{}.wav", dup)
            };
            *dup += 1;

            // Emit TOC entry BEFORE filtering for write so the
            // rebuild order is preserved even for skipped entries
            // (zero-length or mis-aligned).
            let rel = format!("{}/{}", rec.name, filename);
            writeln!(
                toc,
                "    {{ seq_id = 0x{:x}, file = {rel:?} }},",
                e.seq_id,
            )
            .ok();

            if e.length == 0 {
                continue;
            }
            let block_size = audio::BLOCK_BYTES_PER_CHANNEL * e.channels as usize;
            if (e.length as usize) % block_size != 0 {
                continue;
            }
            file.seek(SeekFrom::Start(e.file_offset))?;
            let mut buf = vec![0u8; e.length as usize];
            if file.read_exact(&mut buf).is_err() {
                continue;
            }
            let pcm = audio::decode(&buf, e.channels);
            let path = map_dir.join(&filename);
            write_wav(&path, &pcm, e.channels, e.sample_rate)?;
            written += 1;
        }
        toc.push_str("]\n");
    }
    std::fs::write(dst_dir.join("toc.toml"), toc)
        .wrap_err_with(|| format!("write toc.toml in {dst_dir:?}"))?;
    Ok(written)
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
