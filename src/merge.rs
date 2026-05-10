//! Cross-pair merge of partially-loaded `Linker`s.
//!
//! Each `(common.lin, session.lin)` pair produces a `LinearFileDecoder` whose
//! linkers cover only the exports the recorded session touched. Running many
//! pairs and unioning their `captured.bodies` (and the parsed `objects` that
//! shadow them) yields a per-package picture closer to the authoring-time
//! superset, suitable for re-emit by `ser.rs`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{self, BufWriter, Cursor, Write};
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use byteorder::LittleEndian;

use crate::de::{LinSource, Linker, LinearFileDecoder};

#[derive(Default)]
pub struct MergedLinkers {
    pub linkers: HashMap<String, Rc<RefCell<Linker>>>,
    pub filenames: HashMap<String, String>,
    /// Pair label that first inserted each package, so diagnostics can
    /// name both sides of a conflict.
    pub origin_labels: HashMap<String, String>,
    /// Map-dir name of the pair that first inserted each package. Used
    /// to suppress the canonical-side group from variant emission so
    /// we never emit a variant identical to the canonical itself.
    pub origin_groups: HashMap<String, String>,
    pub bodies_grafted: usize,
    pub body_mismatches: Vec<BodyMismatch>,
    pub table_skews: Vec<TableSkew>,
    /// Divergent variants of a package, keyed by (package_name,
    /// group_name). When fold_pair_into sees a body byte mismatch on
    /// a same-shape package, it records the divergent src linker
    /// under its map-dir group so write-time can emit it as
    /// `<pkg>.<group>.<ext>`. Multiple session pairs from the same
    /// map dir collapse to one variant entry.
    pub variants: HashMap<String, HashMap<String, Rc<RefCell<Linker>>>>,
}

#[derive(Debug, Clone)]
pub struct BodyMismatch {
    pub package: String,
    pub export_index: usize,
    pub dst_label: String,
    pub src_label: String,
    pub dst_len: usize,
    pub src_len: usize,
    pub first_diff_at: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct TableSkew {
    pub package: String,
    pub dst_label: String,
    pub src_label: String,
    pub dst_counts: TableCounts,
    pub src_counts: TableCounts,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableCounts {
    pub names: usize,
    pub imports: usize,
    pub exports: usize,
}

impl MergedLinkers {
    pub fn new() -> Self {
        Self::default()
    }
}

fn is_lin(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("lin"))
        .unwrap_or(false)
}

fn is_named(path: &Path, name: &str) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.eq_ignore_ascii_case(name))
        .unwrap_or(false)
}

/// Build pairs from one map dir's flat entry list. Returns
/// `(common.lin, session.lin)` for every `*.lin` entry that is not
/// `common.lin`. Empty if `common.lin` is absent.
pub fn pairs_from_map_dir_entries(entries: &[PathBuf]) -> Vec<(PathBuf, PathBuf)> {
    let common = match entries.iter().find(|p| is_lin(p) && is_named(p, "common.lin")) {
        Some(c) => c.clone(),
        None => return Vec::new(),
    };
    entries
        .iter()
        .filter(|p| is_lin(p) && !is_named(p, "common.lin"))
        .map(|s| (common.clone(), s.clone()))
        .collect()
}

#[derive(Debug, Default)]
pub struct MergeReport {
    pub pairs_scheduled: usize,
    pub pairs_panicked: usize,
    /// Pairs that ran via `decode_linear_file` against a recorded I/O
    /// trace. The remainder ran via `decode_unchecked`.
    pub traces_used: usize,
    pub unique_packages: usize,
    pub body_entries: usize,
    pub bodies_grafted: usize,
    pub body_mismatches: usize,
    pub table_skews: usize,
}

impl MergeReport {
    pub fn print_summary(&self) {
        eprintln!("merge summary");
        eprintln!("  pairs scheduled:      {}", self.pairs_scheduled);
        eprintln!("  pairs trace-driven:   {}", self.traces_used);
        eprintln!("  pairs panicked:       {}", self.pairs_panicked);
        eprintln!("  unique packages:      {}", self.unique_packages);
        eprintln!("  body entries:         {}", self.body_entries);
        eprintln!("  bodies grafted:       {}", self.bodies_grafted);
        eprintln!("  body byte mismatches: {}", self.body_mismatches);
        eprintln!("  table skews:          {}", self.table_skews);
    }
}

struct PairOutcome {
    linkers: HashMap<String, Rc<RefCell<Linker>>>,
    filenames: HashMap<String, String>,
    panicked: bool,
    /// `true` if this pair ran via `decode_linear_file` against a recorded
    /// I/O trace (engine-faithful). `false` if it ran via
    /// `decode_unchecked` (best-effort cascade replay).
    used_trace: bool,
}

fn read_and_decompress_lin(path: &Path) -> io::Result<Vec<u8>> {
    let f = std::fs::File::open(path)?;
    let mmap = unsafe { memmap2::Mmap::map(&f)? };
    let mut slice: &[u8] = &mmap[..];
    crate::de::decompress_linear_file::<LittleEndian, _>(&mut slice)
}

/// Effective end-of-data for a decompressed `.lin`: the declared
/// `uncompressed_data_size` from metadata block 0. Bytes past that
/// point are zlib block alignment padding (consistently ending in a
/// `0xb3` sentinel) that the engine never reads. Capping `LinReader`
/// at this value makes the cross-source auto-advance fire on the
/// engine's logical end-of-data; using it as the denominator in the
/// unread-tail check filters out the padding so the warning reflects
/// real coverage gaps, not file-format noise.
fn read_and_decompress_lin_with_size(path: &Path) -> io::Result<(Vec<u8>, u64)> {
    let f = std::fs::File::open(path)?;
    let mmap = unsafe { memmap2::Mmap::map(&f)? };
    let mut slice: &[u8] = &mmap[..];
    let (data, declared) =
        crate::de::decompress_linear_file_with_size::<LittleEndian, _>(&mut slice)?;
    Ok((data, declared as u64))
}

/// Index every `reads.json.*` (trace) under `trace_dir` by the session
/// basename of its target map. The session basename is derived from the
/// trace's own `file_load_order` last `Maps\\<level>.unr` entry, so the
/// trace filename can be anything (`reads.json.menu.bak`,
/// `reads.json.training.bak`, `reads.json.003_DefenseMinistry_..._.bak`,
/// or even the live `reads.json` itself). Each session basename is
/// matched case-insensitively to the corresponding `.lin` file in the
/// game tree.
fn index_traces(trace_dir: &Path) -> HashMap<String, PathBuf> {
    let mut by_session: HashMap<String, PathBuf> = HashMap::new();
    let entries = match std::fs::read_dir(trace_dir) {
        Ok(e) => e,
        Err(_) => return by_session,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        if !(name == "reads.json"
            || (name.starts_with("reads.json.") && !name.contains("unknown")))
        {
            continue;
        }
        let session = match level_basename_from_trace(&path) {
            Some(s) => s,
            None => {
                tracing::debug!("trace {path:?}: could not derive level basename, skipping");
                continue;
            }
        };
        // First trace wins for a given session. The user can disambiguate
        // by deleting / renaming the loser; we just log.
        by_session.entry(session).or_insert(path);
    }
    by_session
}

/// Read the `file_load_order` array from a trace JSON and return the
/// level package's basename (the entry under `..\\Maps\\<basename>.unr`).
/// Streams just the first 64 KiB of the file so we don't materialise the
/// 500 MiB trace bodies during indexing — `file_load_order` is the first
/// top-level field in every recorded trace.
fn level_basename_from_trace(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; 64 * 1024];
    let n = f.read(&mut buf).ok()?;
    let s = std::str::from_utf8(&buf[..n]).ok()?;
    // Look for `Maps\\<name>.unr` (escaped backslash in JSON). Examples:
    //   "..\\Maps\\menu\\menu.unr"
    //   "..\\Maps\\1_1_0Tbilisi.unr"
    //   "..\\Maps\\0_0_2_Training.unr"
    let pat = "Maps\\\\";
    let mut i = 0;
    while let Some(off) = s[i..].find(pat) {
        let start = i + off + pat.len();
        let rest = &s[start..];
        let end = rest.find(".unr")?;
        let segment = &rest[..end];
        // Take the last `\\`-separated segment so `menu\\menu` → `menu`.
        let basename = segment.rsplit("\\\\").next().unwrap_or(segment);
        if !basename.is_empty() {
            return Some(basename.to_string());
        }
        i = start + end;
    }
    None
}

fn locate_trace_for_pair(traces: &HashMap<String, PathBuf>, session_path: &Path) -> Option<PathBuf> {
    let session_stem = session_path
        .file_stem()
        .and_then(|s| s.to_str())?
        .to_string();
    // Direct name match (case-insensitive lookup).
    for (k, v) in traces {
        if k.eq_ignore_ascii_case(&session_stem) {
            return Some(v.clone());
        }
    }
    None
}

/// Threshold below which we don't bother reporting unread tail bytes.
/// Trace-driven decodes can leave a few bytes of trailing alignment /
/// padding that the recorded session never read; that isn't a missed
/// package, just file-format noise. Real misses are kilobytes-to-MBs.
const UNREAD_TAIL_THRESHOLD: u64 = 4;

/// After a decode finishes, log a warning for any source whose tail
/// went unread by more than `UNREAD_TAIL_THRESHOLD` bytes. Each `.lin`
/// is a concatenation of UE2 packages indexed by `common.lin`'s
/// file_table; a long unread tail means the cascade never reached one
/// or more of those packages, which is exactly the gap we want to
/// investigate (extras the engine would have loaded but our cascade
/// replay didn't trigger).
fn warn_unread_tails(
    common_path: &Path,
    session_path: &Path,
    common_data: &[u8],
    session_data: &[u8],
    common_declared: u64,
    session_declared: u64,
    consumed: &[u64],
) {
    let common_consumed = consumed.first().copied().unwrap_or(0);
    let session_consumed = consumed.get(1).copied().unwrap_or(0);
    let common_tail = common_declared.saturating_sub(common_consumed);
    let session_tail = session_declared.saturating_sub(session_consumed);
    if common_tail > UNREAD_TAIL_THRESHOLD {
        let pkgs = scan_tail_packages(common_data, common_consumed as usize);
        tracing::warn!(
            "unread tail in {common_path:?}: {common_tail} bytes (consumed {common_consumed}/{common_declared}); pkgs in tail: {pkgs:?}; tail bytes: {}",
            tail_hex(common_data, common_consumed as usize, 48)
        );
    }
    if session_tail > UNREAD_TAIL_THRESHOLD {
        let pkgs = scan_tail_packages(session_data, session_consumed as usize);
        let trailing_pkg_offset = pkgs.last().map(|(o, _)| *o);
        let pre_pkg_bytes = trailing_pkg_offset
            .map(|off| {
                let from = off.saturating_sub(32);
                tail_hex(session_data, from, 32)
            })
            .unwrap_or_default();
        tracing::warn!(
            "unread tail in {session_path:?}: {session_tail} bytes (consumed {session_consumed}/{session_declared}); pkgs: {pkgs:?}; pre-pkg bytes: {pre_pkg_bytes}; tail head: {}",
            tail_hex(session_data, session_consumed as usize, 32)
        );
    }
}

/// Scan the unread region `data[from..]` for UE2 package starts
/// (PKG_TAG = `0xc1 0x83 0x2a 0x9e` in little-endian) and try to read
/// each package's first non-`None` name from its names table. Returns
/// `(absolute_offset, name_or_marker)` pairs. The first non-`None`
/// name is usually the most diagnostic of the package's identity
/// (e.g. `"Camera"`, `"S0_0_3Voice"`); falls back to the raw offset
/// if no package can be parsed.
///
/// This is best-effort: a heuristic scan, not a full parser. False
/// positives are possible if the tail bytes happen to contain the
/// magic, but in practice PKG_TAG is rare enough in body data that
/// matches are real package starts.
fn scan_tail_packages(data: &[u8], from: usize) -> Vec<(usize, String)> {
    const PKG_TAG_LE: [u8; 4] = [0xc1, 0x83, 0x2a, 0x9e];
    let mut out = Vec::new();
    if from >= data.len() {
        return out;
    }
    let mut i = from;
    while i + 4 <= data.len() {
        if data[i..i + 4] == PKG_TAG_LE {
            let name = first_real_name_at_pkg(data, i).unwrap_or_else(|| "?".to_string());
            out.push((i, name));
            i += 4; // step past the magic
            if out.len() >= 32 {
                out.push((data.len(), "...truncated".to_string()));
                break;
            }
        } else {
            i += 1;
        }
    }
    out
}

/// Try to parse a package starting at `data[pkg_offset]` (PKG_TAG)
/// and return the first non-`None` entry in its names table, which
/// is usually the package's own identity name. Returns `None` if
/// the bytes don't look like a valid header.
fn first_real_name_at_pkg(data: &[u8], pkg_offset: usize) -> Option<String> {
    use std::io::Cursor;
    let mut cur = Cursor::new(&data[pkg_offset..]);
    let pkg = crate::de::try_parse_package_at::<byteorder::LittleEndian>(data, pkg_offset)?;
    let _ = &mut cur;
    pkg.names
        .iter()
        .find(|n| !n.name.is_empty() && !n.name.eq_ignore_ascii_case("None"))
        .map(|n| n.name.clone())
}

/// Hex-dump up to `max_bytes` from `data[from..]`. The unread tail
/// often starts with a recognisable UE2 package magic, a known string
/// (`"None"`, etc.), or trailing zeros, all easy to spot in a hex
/// dump. Caps at `max_bytes` to keep the log line readable.
fn tail_hex(data: &[u8], from: usize, max_bytes: usize) -> String {
    let from = from.min(data.len());
    let end = (from + max_bytes).min(data.len());
    let slice = &data[from..end];
    let mut s = String::with_capacity(slice.len() * 3 + 8);
    for (i, b) in slice.iter().enumerate() {
        if i > 0 && i % 16 == 0 {
            s.push(' ');
        }
        s.push_str(&format!("{b:02x} "));
    }
    if end < data.len() {
        s.push_str("...");
    }
    s
}

fn run_pair(
    common_path: &Path,
    session_path: &Path,
    trace_path: Option<&Path>,
) -> io::Result<PairOutcome> {
    let (common_data, common_size) = read_and_decompress_lin_with_size(common_path)?;
    let (session_data, session_size) = read_and_decompress_lin_with_size(session_path)?;
    // Keep a copy of the decompressed bytes so `warn_unread_tails` can
    // hex-dump the start of any unread region after the decoder has
    // consumed the originals.
    let common_data_for_tail = common_data.clone();
    let session_data_for_tail = session_data.clone();

    if let Some(trace_path) = trace_path {
        // Trace-driven path: the recorded I/O ops drive `LinReader`'s
        // every read and seek, so the cascade order matches the engine's
        // exactly. This is the path that recovers the full cross-source
        // texture coverage — `decode_unchecked`'s natural cascade only
        // hits ~71/110 character textures because src0's cursor doesn't
        // line up for stage-2-cross-source preloads without trace data.
        let f = std::fs::File::open(trace_path).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("failed to open trace {trace_path:?}: {e}"),
            )
        })?;
        let reader = io::BufReader::new(f);
        let mut metadata: crate::common::ExportedData =
            serde_json::from_reader(reader).map_err(|e| {
                io::Error::new(io::ErrorKind::InvalidData, format!("trace parse: {e}"))
            })?;
        metadata
            .file_reads
            .iter_mut()
            .for_each(|(_k, v)| v.reverse());

        let mut decoder = LinearFileDecoder::<LittleEndian, _>::new_checked(
            vec![Cursor::new(common_data), Cursor::new(session_data)],
            metadata,
        );

        let panicked = std::panic::catch_unwind(AssertUnwindSafe(|| {
            if let Err(e) = decoder.decode_linear_file() {
                tracing::warn!(?session_path, ?trace_path, "decode_linear_file err: {e}");
            }
        }))
        .is_err();

        // Trace match validation. The trace is keyed by session
        // basename ("menu", "1_2_1DefenseMinistry", etc.), which
        // matches across builds even when the underlying `.lin` bytes
        // differ. Replaying a retail trace against proto/demo data
        // panics early (CheckedLinReader's IO-op assertions fire on
        // size/byte mismatches), leaving common.lin barely touched
        // and session.lin completely unread. Detect that and fall
        // back to the unchecked path which doesn't depend on the
        // recorded ops.
        let consumed = decoder.source_consumed_per_source();
        let common_consumed = consumed.first().copied().unwrap_or(0);
        let session_consumed = consumed.get(1).copied().unwrap_or(0);
        let trace_mismatch = panicked
            || session_consumed == 0
            || common_consumed * 2 < common_data_for_tail.len() as u64;
        if trace_mismatch {
            tracing::warn!(
                ?session_path, ?trace_path,
                "trace replay incomplete (panicked={panicked} common={common_consumed}/{} session={session_consumed}/{}); falling back to --no-checked",
                common_data_for_tail.len(),
                session_data_for_tail.len(),
            );
            // Drop the failed decoder and retry in unchecked mode.
            // Re-decompress the .lin sources since the previous
            // decoder consumed them.
        } else {
            warn_unread_tails(
                common_path,
                session_path,
                &common_data_for_tail,
                &session_data_for_tail,
                common_size,
                session_size,
                &decoder.source_consumed_per_source(),
            );

            let linkers = decoder.linkers().clone();
            let filenames = decoder.package_filenames();
            return Ok(PairOutcome {
                linkers,
                filenames,
                panicked,
                used_trace: true,
            });
        }
    }

    // Unchecked path: fresh data sources from the (already-decompressed)
    // tail clones. The trace path above moved its own copies of the
    // data into Cursors, so we use the tail clones here. Re-clone for
    // tail diagnostics so the Cursor's move into `new_unchecked_with_limits`
    // doesn't invalidate the post-decode unread-tail dump.
    let common_for_decode = common_data_for_tail.clone();
    let session_for_decode = session_data_for_tail.clone();
    let mut decoder = LinearFileDecoder::<LittleEndian, _>::new_unchecked(vec![
        LinSource::new(Cursor::new(common_for_decode), common_size),
        LinSource::new(Cursor::new(session_for_decode), session_size),
    ]);

    let panicked = std::panic::catch_unwind(AssertUnwindSafe(|| {
        if let Err(e) = decoder.decode_unchecked() {
            tracing::warn!(?session_path, "decode_unchecked err: {e}");
        }
    }))
    .is_err();

    warn_unread_tails(
        common_path,
        session_path,
        &common_data_for_tail,
        &session_data_for_tail,
        common_size,
        session_size,
        &decoder.source_consumed_per_source(),
    );

    let linkers = decoder.linkers().clone();
    let filenames = decoder.package_filenames();
    Ok(PairOutcome {
        linkers,
        filenames,
        panicked,
        used_trace: false,
    })
}

/// Serialize every merged package to `<output>/packages/<rel_path>`.
pub fn write_packages(
    pkg_out: &Path,
    linkers: &HashMap<String, Rc<RefCell<Linker>>>,
    filenames: &HashMap<String, String>,
) -> io::Result<()> {
    for (name, linker) in linkers {
        let linker = linker.borrow();
        let Some(rel) = filenames.get(&name.to_ascii_lowercase()) else {
            tracing::warn!("package {name:?} not in file_table; skipping");
            continue;
        };
        let path = pkg_out.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let f = std::fs::File::create(&path)?;
        let mut bw = BufWriter::new(f);
        if let Err(e) = crate::ser::serialize_linker_le(&linker, &mut bw) {
            tracing::warn!("failed to serialize {name}: {e}");
        }
        bw.flush()?;
        tracing::info!(
            "wrote {path:?} ({} exports captured)",
            linker.captured.bodies.len()
        );
    }
    Ok(())
}

/// Serialize each divergent variant under `<rel_dir>/<stem>.<group>.<ext>`,
/// preserving both the canonical (first-sighting) and any later
/// authoring-time variants captured from different map dirs. The
/// canonical file is already written by `write_packages`; this writes
/// only the conflicting alternates so consumers see two distinct
/// package files rather than the merge silently dropping one side.
fn write_variants(pkg_out: &Path, merged: &MergedLinkers) -> io::Result<()> {
    for (name, by_group) in &merged.variants {
        let lower = name.to_ascii_lowercase();
        let Some(rel) = merged.filenames.get(&lower) else {
            tracing::warn!("variant {name:?} not in file_table; skipping");
            continue;
        };
        let base_path = pkg_out.join(rel);
        for (group, linker) in by_group {
            let variant_path = insert_group_in_filename(&base_path, group);
            if let Some(parent) = variant_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let f = std::fs::File::create(&variant_path)?;
            let mut bw = BufWriter::new(f);
            let linker = linker.borrow();
            if let Err(e) = crate::ser::serialize_linker_le(&linker, &mut bw) {
                tracing::warn!("failed to serialize variant {name}.{group}: {e}");
            }
            bw.flush()?;
            tracing::info!(
                "wrote variant {variant_path:?} ({} exports captured)",
                linker.captured.bodies.len()
            );
        }
    }
    Ok(())
}

/// `Foo/ESam.unr` + `"012_PresidentialPalace"` → `Foo/ESam.012_PresidentialPalace.unr`.
/// If the filename has no extension, just appends `.<group>`.
fn insert_group_in_filename(path: &Path, group: &str) -> PathBuf {
    let parent = path.parent();
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let new_name = if let Some(dot) = file_name.rfind('.') {
        let (stem, ext) = file_name.split_at(dot);
        format!("{stem}.{group}{ext}")
    } else {
        format!("{file_name}.{group}")
    };
    match parent {
        Some(p) => p.join(new_name),
        None => PathBuf::from(new_name),
    }
}

fn write_diagnostics(path: &Path, merged: &MergedLinkers) -> io::Result<()> {
    let mut f = std::fs::File::create(path)?;
    writeln!(
        f,
        "# body byte mismatches\n# Two pairs captured different bytes for the same export.\n# Common cause: distinct common.lin variants across map dirs (retail SC NTSC ships\n# at least 4 md5-distinct common.lin files), so cross-variant pairs read different\n# authoring-time content. dst (first sighting) wins in the merged output.\n"
    )?;
    for m in &merged.body_mismatches {
        writeln!(
            f,
            "{} | export[{}] | dst={} ({}B) src={} ({}B) first_diff_at={:?}",
            m.package,
            m.export_index,
            m.dst_label,
            m.dst_len,
            m.src_label,
            m.src_len,
            m.first_diff_at,
        )?;
    }
    writeln!(
        f,
        "\n# table skews\n# A package was loaded with different (names, imports, exports) shapes across pairs.\n# Common cause: cascade-order misalignment when authoring-time imports reference\n# packages whose bodies the engine had cached from outside the recorded session.\n# Fold promotes the sighting with the most exports to canonical; smaller-shape\n# sightings are discarded (their bytes belonged to a different package by name).\n"
    )?;
    for s in &merged.table_skews {
        writeln!(
            f,
            "{} | dst={} {:?} src={} {:?}",
            s.package, s.dst_label, s.dst_counts, s.src_label, s.src_counts,
        )?;
    }
    Ok(())
}

pub fn run_merge(game_dir: &Path, output_dir: &Path, trace_dir: Option<&Path>) -> io::Result<MergeReport> {
    let pairs = discover_pairs(game_dir)?;
    let mut report = MergeReport::default();
    report.pairs_scheduled = pairs.len();

    let traces = trace_dir.map(index_traces).unwrap_or_default();
    if !traces.is_empty() {
        eprintln!("indexed {} traces:", traces.len());
        let mut keys: Vec<_> = traces.keys().collect();
        keys.sort();
        for k in keys {
            eprintln!("  {k} -> {:?}", traces[k]);
        }
    }

    let mut merged = MergedLinkers::new();
    let mut traces_used = 0usize;

    for (common, session) in &pairs {
        let label = session
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("?")
            .to_string();
        // Group label = parent dir name (e.g. "012_PresidentialPalace").
        // All session pairs from the same map dir share a `common.lin`
        // and produce the same divergent body content for any
        // conflicted package, so collapsing variants by parent dir
        // avoids emitting one file per session pair when two pairs in
        // the same map carry the same authoring-time variant.
        let group = session
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("?")
            .to_string();
        let trace = locate_trace_for_pair(&traces, session);
        match &trace {
            Some(t) => eprintln!("pair: {common:?} + {session:?} (trace: {t:?})"),
            None => eprintln!("pair: {common:?} + {session:?} (no trace, --no-checked path)"),
        }
        match run_pair(common, session, trace.as_deref()) {
            Ok(out) => {
                if out.panicked {
                    report.pairs_panicked += 1;
                }
                if out.used_trace {
                    traces_used += 1;
                }
                fold_pair_into(&mut merged, &out.linkers, &out.filenames, &label, &group);
            }
            Err(e) => {
                tracing::warn!(?session, "run_pair failed: {e}");
            }
        }
    }
    report.traces_used = traces_used;

    let pkg_out = output_dir.join("packages");
    std::fs::create_dir_all(&pkg_out)?;
    write_packages(&pkg_out, &merged.linkers, &merged.filenames)?;
    write_variants(&pkg_out, &merged)?;

    write_diagnostics(&output_dir.join("merge_diagnostics.txt"), &merged)?;

    report.unique_packages = merged.linkers.len();
    report.body_entries = merged
        .linkers
        .values()
        .map(|l| l.borrow().captured.bodies.len())
        .sum();
    report.bodies_grafted = merged.bodies_grafted;
    report.body_mismatches = merged.body_mismatches.len();
    report.table_skews = merged.table_skews.len();
    Ok(report)
}

/// Walk `<game_dir>/LMaps/*/` and yield pairs for every map dir that
/// has a `common.lin`. Pairs are sorted by `(map_dir, session)` for
/// stable log output.
pub fn discover_pairs(game_dir: &Path) -> std::io::Result<Vec<(PathBuf, PathBuf)>> {
    let lmaps = game_dir.join("LMaps");
    if !lmaps.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("{lmaps:?} is not a directory"),
        ));
    }

    let mut map_dirs: Vec<PathBuf> = std::fs::read_dir(&lmaps)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    map_dirs.sort();

    let mut all = Vec::new();
    for map_dir in map_dirs {
        let entries: Vec<PathBuf> = std::fs::read_dir(&map_dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .collect();
        let mut pairs = pairs_from_map_dir_entries(&entries);
        pairs.sort();
        if pairs.is_empty() {
            tracing::warn!("skipping {map_dir:?}: no common.lin");
        }
        all.extend(pairs);
    }
    Ok(all)
}

pub fn fold_pair_into(
    merged: &mut MergedLinkers,
    pair_linkers: &HashMap<String, Rc<RefCell<Linker>>>,
    pair_filenames: &HashMap<String, String>,
    pair_label: &str,
    group_label: &str,
) {
    for (name, src_rc) in pair_linkers {
        if !merged.linkers.contains_key(name) {
            merged.linkers.insert(name.clone(), src_rc.clone());
            merged
                .origin_labels
                .insert(name.clone(), pair_label.to_string());
            merged
                .origin_groups
                .insert(name.clone(), group_label.to_string());
            // runtime.linkers keys are mixed-case (e.g. "Core"); package_filenames
            // keys are lowercased (e.g. "core"). Bridge with a lowercased lookup,
            // and store under the lowercased key so the writer's normalised lookup
            // still hits.
            let lower = name.to_ascii_lowercase();
            if let Some(rel) = pair_filenames.get(&lower) {
                merged.filenames.insert(lower, rel.clone());
            }
            continue;
        }

        let dst_rc = merged.linkers[name].clone();
        if Rc::ptr_eq(&dst_rc, src_rc) {
            continue;
        }

        let mut dst = dst_rc.borrow_mut();
        let src = src_rc.borrow();

        let dst_counts = TableCounts {
            names: dst.package.names.len(),
            imports: dst.package.imports.len(),
            exports: dst.package.exports.len(),
        };
        let src_counts = TableCounts {
            names: src.package.names.len(),
            imports: src.package.imports.len(),
            exports: src.package.exports.len(),
        };
        if dst_counts != src_counts {
            let dst_label = merged
                .origin_labels
                .get(name)
                .cloned()
                .unwrap_or_else(|| "<unknown>".to_string());
            merged.table_skews.push(TableSkew {
                package: name.clone(),
                dst_label,
                src_label: pair_label.to_string(),
                dst_counts,
                src_counts,
            });
            // Empirically, when the cascade misaligns it tends to read
            // some other package's header into a Linker keyed under the
            // wrong name. The misaligned shape is usually small (e.g.
            // (15,2,8)) while the real package's shape is larger.
            // Promote the larger-shape sighting to canonical and discard
            // the smaller one's bodies; those bodies belonged to a
            // different authoring-time package and would corrupt the
            // output if grafted into the canonical exports.
            if src_counts.exports > dst_counts.exports {
                drop(dst);
                drop(src);
                merged.linkers.insert(name.clone(), src_rc.clone());
                merged
                    .origin_labels
                    .insert(name.clone(), pair_label.to_string());
            }
            continue;
        }

        let mut had_mismatch = false;
        for (idx, body) in &src.captured.bodies {
            match dst.captured.bodies.get(idx) {
                None => {
                    dst.captured.bodies.insert(*idx, body.clone());
                    merged.bodies_grafted += 1;
                }
                Some(existing) if existing == body => {}
                Some(existing) => {
                    had_mismatch = true;
                    let first_diff_at = existing
                        .iter()
                        .zip(body.iter())
                        .position(|(a, b)| a != b)
                        .or_else(|| {
                            if existing.len() != body.len() {
                                Some(existing.len().min(body.len()))
                            } else {
                                None
                            }
                        });
                    merged.body_mismatches.push(BodyMismatch {
                        package: name.clone(),
                        export_index: *idx,
                        dst_label: merged
                            .origin_labels
                            .get(name)
                            .cloned()
                            .unwrap_or_else(|| "<unknown>".to_string()),
                        src_label: pair_label.to_string(),
                        dst_len: existing.len(),
                        src_len: body.len(),
                        first_diff_at,
                    });
                }
            }
        }

        for (idx, obj) in &src.objects {
            dst.objects.entry(*idx).or_insert_with(|| obj.clone());
        }

        // Record this src as a divergent variant if at least one body
        // disagreed AND the src is from a different map-dir group than
        // canonical. Multiple session pairs in the same group collapse
        // to one variant entry (first sighting wins per group). The
        // variant linker is the unmodified src clone; its
        // captured.bodies have the divergent versions of the
        // conflicting exports plus the same matching versions for
        // everything else; on write-back we serialize the whole thing
        // under `<pkg>.<group>.<ext>` so consumers see two distinct
        // package files preserving both authoring-time variants.
        if had_mismatch
            && merged
                .origin_groups
                .get(name)
                .is_none_or(|g| g != group_label)
        {
            drop(dst);
            drop(src);
            merged
                .variants
                .entry(name.clone())
                .or_default()
                .entry(group_label.to_string())
                .or_insert_with(|| src_rc.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::de::{
        CapturedBytes, GenerationInfo, Import, Name, ObjectExport, PackageHeader, RawPackage,
    };

    fn synth_header() -> PackageHeader {
        PackageHeader {
            version: 0,
            flags: 0,
            name_count: 0,
            name_offset: 0,
            export_count: 0,
            export_offset: 0,
            import_count: 0,
            import_offset: 0,
            unk: 0,
            unknown_data: Vec::new(),
            guid_a: 0,
            guid_b: 0,
            guid_c: 0,
            guid_d: 0,
            generations: Vec::<GenerationInfo>::new(),
        }
    }

    fn synth_linker(
        name: &str,
        names_len: usize,
        imports_len: usize,
        exports_len: usize,
        bodies: Vec<(usize, Vec<u8>)>,
    ) -> Linker {
        Linker {
            objects: Default::default(),
            name: name.to_string(),
            package: RawPackage {
                header: synth_header(),
                names: (0..names_len)
                    .map(|_| Name {
                        name: String::new(),
                        flags: 0,
                    })
                    .collect(),
                imports: (0..imports_len)
                    .map(|_| Import {
                        class_package: 0,
                        class_name: 0,
                        package_index: 0,
                        object_name: 0,
                    })
                    .collect(),
                exports: (0..exports_len)
                    .map(|_| ObjectExport {
                        class_index: 0,
                        super_index: 0,
                        package_index: 0,
                        object_name: 0,
                        object_flags: 0,
                        serial_size: 0,
                        serial_offset: 0,
                    })
                    .collect(),
            },
            reader_offset: 0,
            source_start: 0,
            source_idx: 0,
            captured: CapturedBytes {
                bodies: bodies.into_iter().collect(),
            },
        }
    }

    fn pair(
        linkers: Vec<(&str, Linker)>,
        filenames: Vec<(&str, &str)>,
    ) -> (
        HashMap<String, Rc<RefCell<Linker>>>,
        HashMap<String, String>,
    ) {
        let l = linkers
            .into_iter()
            .map(|(k, v)| (k.to_string(), Rc::new(RefCell::new(v))))
            .collect();
        let f = filenames
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        (l, f)
    }

    #[test]
    fn pairs_from_dir_finds_common_and_sessions() {
        let entries = vec![
            PathBuf::from("/g/LMaps/001/common.lin"),
            PathBuf::from("/g/LMaps/001/0_0_2_Training.lin"),
            PathBuf::from("/g/LMaps/001/0_0_3_Training.lin"),
            PathBuf::from("/g/LMaps/001/0_0_2_Training.bik"),
        ];
        let mut pairs = pairs_from_map_dir_entries(&entries);
        pairs.sort();
        assert_eq!(
            pairs,
            vec![
                (
                    PathBuf::from("/g/LMaps/001/common.lin"),
                    PathBuf::from("/g/LMaps/001/0_0_2_Training.lin"),
                ),
                (
                    PathBuf::from("/g/LMaps/001/common.lin"),
                    PathBuf::from("/g/LMaps/001/0_0_3_Training.lin"),
                ),
            ],
        );
    }

    #[test]
    fn pairs_from_dir_returns_empty_when_common_missing() {
        let entries = vec![
            PathBuf::from("/g/LMaps/foo/menu.lin"),
            PathBuf::from("/g/LMaps/foo/foo.bik"),
        ];
        assert!(pairs_from_map_dir_entries(&entries).is_empty());
    }

    #[test]
    fn pairs_from_dir_excludes_subdir_entries() {
        // entries should already be filtered to top-level files; the
        // walker is responsible for skipping subdirs like `French/`.
        let entries = vec![
            PathBuf::from("/g/LMaps/000_menu/common.lin"),
            PathBuf::from("/g/LMaps/000_menu/menu.lin"),
        ];
        let pairs = pairs_from_map_dir_entries(&entries);
        assert_eq!(pairs.len(), 1);
    }

    #[test]
    fn fold_first_sighting_inserts_base() {
        let mut merged = MergedLinkers::new();
        let (linkers, fnames) = pair(
            vec![("hud", synth_linker("HUD", 1, 0, 1, vec![(0, vec![0xAA, 0xBB])]))],
            vec![("hud", "Textures/HUD.utx")],
        );
        fold_pair_into(&mut merged, &linkers, &fnames, "pair-A", "group-A");

        assert!(merged.linkers.contains_key("hud"));
        assert_eq!(
            merged.filenames.get("hud").map(String::as_str),
            Some("Textures/HUD.utx"),
        );
        let l = merged.linkers["hud"].borrow();
        assert_eq!(l.captured.bodies.get(&0), Some(&vec![0xAA, 0xBB]));
        assert_eq!(merged.bodies_grafted, 0);
        assert!(merged.body_mismatches.is_empty());
        assert!(merged.table_skews.is_empty());
    }

    #[test]
    fn fold_filename_lookup_is_case_insensitive() {
        // Mirrors reality: runtime.linkers uses mixed-case keys
        // ("Core", "Engine"), package_filenames returns lowercased keys
        // ("core", "engine"). The fold must bridge these.
        let mut merged = MergedLinkers::new();
        let (linkers, fnames) = pair(
            vec![("Core", synth_linker("Core", 1, 0, 1, vec![]))],
            vec![("core", "Core.u")],
        );
        fold_pair_into(&mut merged, &linkers, &fnames, "pair-A", "group-A");

        // write_packages does `filenames.get(&name.to_ascii_lowercase())`
        // so storing the filename under the lowercased key keeps that path
        // working unchanged.
        assert_eq!(
            merged.filenames.get("core").map(String::as_str),
            Some("Core.u"),
        );
    }

    #[test]
    fn fold_first_wins_on_overlap_identical() {
        let mut merged = MergedLinkers::new();
        let (a_lns, a_fns) = pair(
            vec![("hud", synth_linker("HUD", 4, 0, 4, vec![(0, vec![0xAA, 0xBB])]))],
            vec![("hud", "Textures/HUD.utx")],
        );
        fold_pair_into(&mut merged, &a_lns, &a_fns, "pair-A", "group-A");

        let (b_lns, b_fns) = pair(
            vec![("hud", synth_linker("HUD", 4, 0, 4, vec![(0, vec![0xAA, 0xBB])]))],
            vec![("hud", "Textures/HUD.utx")],
        );
        fold_pair_into(&mut merged, &b_lns, &b_fns, "pair-B", "group-B");

        let l = merged.linkers["hud"].borrow();
        assert_eq!(l.captured.bodies.get(&0), Some(&vec![0xAA, 0xBB]));
        assert_eq!(merged.bodies_grafted, 0);
        assert!(merged.body_mismatches.is_empty());
    }

    #[test]
    fn fold_records_mismatch_on_overlap_diverging() {
        let mut merged = MergedLinkers::new();
        let (a_lns, a_fns) = pair(
            vec![("hud", synth_linker("HUD", 4, 0, 4, vec![(1, vec![0x01, 0x02, 0x03])]))],
            vec![("hud", "Textures/HUD.utx")],
        );
        fold_pair_into(&mut merged, &a_lns, &a_fns, "pair-A", "group-A");

        let (b_lns, b_fns) = pair(
            vec![("hud", synth_linker("HUD", 4, 0, 4, vec![(1, vec![0x01, 0xFF, 0x03])]))],
            vec![("hud", "Textures/HUD.utx")],
        );
        fold_pair_into(&mut merged, &b_lns, &b_fns, "pair-B", "group-B");

        let l = merged.linkers["hud"].borrow();
        assert_eq!(l.captured.bodies.get(&1), Some(&vec![0x01, 0x02, 0x03]));
        assert_eq!(merged.body_mismatches.len(), 1);
        let m = &merged.body_mismatches[0];
        assert_eq!(m.package, "hud");
        assert_eq!(m.export_index, 1);
        assert_eq!(m.src_label, "pair-B");
        assert_eq!(m.first_diff_at, Some(1));
    }

    #[test]
    fn fold_promotes_on_larger_shape() {
        // Mirrors the retail Kalinatek_OBJ scenario: an early pair sees
        // a small (15,2,8) "stub" for a package whose real shape is
        // (414,221,252). The real shape arrives in a later pair. We
        // want the bigger-shape sighting to become canonical.
        let mut merged = MergedLinkers::new();
        let (a_lns, a_fns) = pair(
            vec![("hud", synth_linker("HUD", 4, 0, 4, vec![(0, vec![0xAA])]))],
            vec![("hud", "Textures/HUD.utx")],
        );
        fold_pair_into(&mut merged, &a_lns, &a_fns, "pair-A", "group-A");

        let (b_lns, b_fns) = pair(
            vec![(
                "hud",
                synth_linker("HUD", 9, 0, 8, vec![(7, vec![0xFF])]),
            )],
            vec![("hud", "Textures/HUD.utx")],
        );
        fold_pair_into(&mut merged, &b_lns, &b_fns, "pair-B", "group-B");

        let l = merged.linkers["hud"].borrow();
        // Canonical now has 8 exports (B's shape).
        assert_eq!(l.package.exports.len(), 8);
        // B's body at idx 7 is in canonical.
        assert_eq!(l.captured.bodies.get(&7), Some(&vec![0xFF]));
        // A's body at idx 0 is DISCARDED (it was for a different package shape).
        assert!(l.captured.bodies.get(&0).is_none());
        assert_eq!(merged.table_skews.len(), 1);
        // origin_label moves to the promoted pair.
        assert_eq!(merged.origin_labels.get("hud").map(String::as_str), Some("pair-B"));
    }

    #[test]
    fn fold_keeps_dst_when_src_smaller() {
        // Inverse of promotion: first pair has the real shape, later
        // pair has a smaller stub. Keep the canonical, discard stub.
        let mut merged = MergedLinkers::new();
        let (a_lns, a_fns) = pair(
            vec![(
                "hud",
                synth_linker("HUD", 9, 0, 8, vec![(7, vec![0xAA])]),
            )],
            vec![("hud", "Textures/HUD.utx")],
        );
        fold_pair_into(&mut merged, &a_lns, &a_fns, "pair-A", "group-A");

        let (b_lns, b_fns) = pair(
            vec![("hud", synth_linker("HUD", 4, 0, 4, vec![(0, vec![0xFF])]))],
            vec![("hud", "Textures/HUD.utx")],
        );
        fold_pair_into(&mut merged, &b_lns, &b_fns, "pair-B", "group-B");

        let l = merged.linkers["hud"].borrow();
        assert_eq!(l.package.exports.len(), 8);
        assert_eq!(l.captured.bodies.get(&7), Some(&vec![0xAA]));
        // B's body at idx 0 is discarded.
        assert!(l.captured.bodies.get(&0).is_none());
        assert_eq!(merged.table_skews.len(), 1);
        assert_eq!(merged.origin_labels.get("hud").map(String::as_str), Some("pair-A"));
    }

    #[test]
    fn fold_unions_objects_with_bodies() {
        let mut merged = MergedLinkers::new();

        let (a_lns, a_fns) = pair(
            vec![("hud", synth_linker("HUD", 4, 0, 4, vec![(0, vec![0xAA])]))],
            vec![("hud", "Textures/HUD.utx")],
        );
        fold_pair_into(&mut merged, &a_lns, &a_fns, "pair-A", "group-A");

        // Build a second pair with a body at idx=2 and a parallel object entry.
        let mut second = synth_linker("HUD", 4, 0, 4, vec![(2, vec![0xCC])]);
        let dummy: Rc<RefCell<dyn crate::object::UnrealObject>> =
            Rc::new(RefCell::new(crate::object::uobject::Object::default()));
        second.objects.insert(crate::de::ExportIndex(2), dummy);
        let (b_lns, b_fns) = pair(
            vec![("hud", second)],
            vec![("hud", "Textures/HUD.utx")],
        );
        fold_pair_into(&mut merged, &b_lns, &b_fns, "pair-B", "group-B");

        let l = merged.linkers["hud"].borrow();
        assert!(l.captured.bodies.contains_key(&2));
        assert!(l.objects.contains_key(&crate::de::ExportIndex(2)));
        assert_eq!(merged.bodies_grafted, 1);
    }

    #[test]
    fn fold_records_variant_on_cross_group_mismatch() {
        // Cross-group divergence: pair-A from group "menu" wins canonical;
        // pair-B from group "012_PresidentialPalace" disagrees on a body.
        // The src linker should be recorded as a variant under its group
        // label so write_variants emits a `<pkg>.012_PresidentialPalace.utx`.
        let mut merged = MergedLinkers::new();
        let (a_lns, a_fns) = pair(
            vec![("ESam", synth_linker("ESam", 4, 0, 4, vec![(1, vec![0xAA, 0xBB])]))],
            vec![("esam", "Animations/ESam.ukx")],
        );
        fold_pair_into(&mut merged, &a_lns, &a_fns, "menu.lin", "menu");

        let (b_lns, b_fns) = pair(
            vec![("ESam", synth_linker("ESam", 4, 0, 4, vec![(1, vec![0xAA, 0xCC])]))],
            vec![("esam", "Animations/ESam.ukx")],
        );
        fold_pair_into(
            &mut merged,
            &b_lns,
            &b_fns,
            "5_1_1_PresidentialPalace.lin",
            "012_PresidentialPalace",
        );

        let by_group = merged
            .variants
            .get("ESam")
            .expect("variant entry recorded for ESam");
        assert!(
            by_group.contains_key("012_PresidentialPalace"),
            "src group label keys the variant, not the canonical group"
        );
        assert!(
            !by_group.contains_key("menu"),
            "canonical group never appears as a variant"
        );
    }

    #[test]
    fn fold_skips_variant_on_same_group_mismatch() {
        // Two pairs from the same map dir disagree on a body. This
        // shouldn't happen in practice (same dir shares a common.lin)
        // but the guard exists so we never emit a variant whose group
        // matches canonical's group.
        let mut merged = MergedLinkers::new();
        let (a_lns, a_fns) = pair(
            vec![("ESam", synth_linker("ESam", 4, 0, 4, vec![(1, vec![0xAA])]))],
            vec![("esam", "Animations/ESam.ukx")],
        );
        fold_pair_into(&mut merged, &a_lns, &a_fns, "session-a.lin", "same-group");

        let (b_lns, b_fns) = pair(
            vec![("ESam", synth_linker("ESam", 4, 0, 4, vec![(1, vec![0xBB])]))],
            vec![("esam", "Animations/ESam.ukx")],
        );
        fold_pair_into(&mut merged, &b_lns, &b_fns, "session-b.lin", "same-group");

        assert!(
            merged.variants.get("ESam").is_none(),
            "no variant when src group equals canonical group"
        );
        assert_eq!(merged.body_mismatches.len(), 1);
    }

    #[test]
    fn fold_unions_disjoint_bodies() {
        let mut merged = MergedLinkers::new();

        let (a_lns, a_fns) = pair(
            vec![("hud", synth_linker("HUD", 4, 0, 4, vec![(0, vec![0xAA])]))],
            vec![("hud", "Textures/HUD.utx")],
        );
        fold_pair_into(&mut merged, &a_lns, &a_fns, "pair-A", "group-A");

        let (b_lns, b_fns) = pair(
            vec![("hud", synth_linker("HUD", 4, 0, 4, vec![(2, vec![0xCC, 0xDD])]))],
            vec![("hud", "Textures/HUD.utx")],
        );
        fold_pair_into(&mut merged, &b_lns, &b_fns, "pair-B", "group-B");

        let l = merged.linkers["hud"].borrow();
        assert_eq!(l.captured.bodies.get(&0), Some(&vec![0xAA]));
        assert_eq!(l.captured.bodies.get(&2), Some(&vec![0xCC, 0xDD]));
        assert_eq!(merged.bodies_grafted, 1);
        assert!(merged.body_mismatches.is_empty());
    }
}
