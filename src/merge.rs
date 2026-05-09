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

use crate::de::{Linker, LinearFileDecoder};

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
}

fn read_and_decompress_lin(path: &Path) -> io::Result<Vec<u8>> {
    let f = std::fs::File::open(path)?;
    let mmap = unsafe { memmap2::Mmap::map(&f)? };
    let mut slice: &[u8] = &mmap[..];
    crate::de::decompress_linear_file::<LittleEndian, _>(&mut slice)
}

fn run_pair(common_path: &Path, session_path: &Path) -> io::Result<PairOutcome> {
    let common_data = read_and_decompress_lin(common_path)?;
    let session_data = read_and_decompress_lin(session_path)?;

    // Match bin.rs: scan map.lin for package names referenced as
    // top-level imports, so `verify_imports` knows to attempt
    // `load_linker` on map-specific packages (Camera, clock, etc.)
    // not in COMMON_LIN_PACKAGES. Without this, cascade reads
    // misalign and downstream parsers (script bytecode, StaticMesh
    // index buffers) hit garbage bytes.
    let extra_packages = crate::de::discover_secondary_package_names(&session_data);

    let mut decoder = LinearFileDecoder::<LittleEndian, _>::new_unchecked(vec![
        Cursor::new(common_data),
        Cursor::new(session_data),
    ]);
    for name in extra_packages {
        decoder.runtime_mut().present_packages.insert(name);
    }

    let panicked = std::panic::catch_unwind(AssertUnwindSafe(|| {
        if let Err(e) = decoder.decode_unchecked() {
            tracing::warn!(?session_path, "decode_unchecked err: {e}");
        }
    }))
    .is_err();

    let linkers = decoder.linkers().clone();
    let filenames = decoder.package_filenames();
    Ok(PairOutcome {
        linkers,
        filenames,
        panicked,
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

pub fn run_merge(game_dir: &Path, output_dir: &Path) -> io::Result<MergeReport> {
    let pairs = discover_pairs(game_dir)?;
    let mut report = MergeReport::default();
    report.pairs_scheduled = pairs.len();

    let mut merged = MergedLinkers::new();

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
        eprintln!("pair: {common:?} + {session:?}");
        match run_pair(common, session) {
            Ok(out) => {
                if out.panicked {
                    report.pairs_panicked += 1;
                }
                fold_pair_into(&mut merged, &out.linkers, &out.filenames, &label, &group);
            }
            Err(e) => {
                tracing::warn!(?session, "run_pair failed: {e}");
            }
        }
    }

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
