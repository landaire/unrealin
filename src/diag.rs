//! Diagnostics for the static-recompile pipeline. Read-only passes that
//! verify invariants we expect the deserialize/serialize chain to hold.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use byteorder::ByteOrder;

use crate::de::Linker;
use crate::object::UObjectKind;
use crate::object::builtins::Struct;
use crate::object::internal::script;

/// Per-export round-trip outcome for a UStruct subtype. `script_capture`
/// is the verbatim source bytes consumed by `Struct::deserialize`'s
/// script section; `serialize_expr(parsed_script)` is the canonical
/// re-emit. The two are EXPECTED to diverge by exactly the peek-prefix
/// bytes that `handle_optional_debug_info` consumes from the .lin
/// without recording into the tree (see comment there for the
/// rationale). A captured length larger than the serialized length by
/// `1` or `5` per peek-and-rewind site is normal for any function body
/// containing at least one function-call expression; only mismatches
/// of a different shape (length-shorter-than-canonical, byte-level
/// diff at non-peek positions) point at real parse drift.
#[derive(Debug)]
pub struct ScriptRoundtripMismatch {
    pub package: String,
    pub export_name: String,
    pub class_name: String,
    pub captured_len: usize,
    pub serialized_len: usize,
    pub first_diff_at: Option<usize>,
}

#[derive(Debug, Default)]
pub struct ScriptRoundtripStats {
    pub structs_with_script: usize,
    pub matching: usize,
    pub mismatches: Vec<ScriptRoundtripMismatch>,
}

impl ScriptRoundtripStats {
    pub fn print_summary(&self, max_examples: usize) {
        println!(
            "script roundtrip: {}/{} match",
            self.matching, self.structs_with_script
        );
        if self.mismatches.is_empty() {
            return;
        }
        println!(
            "  {} mismatches; first {}:",
            self.mismatches.len(),
            max_examples.min(self.mismatches.len())
        );
        for m in self.mismatches.iter().take(max_examples) {
            println!(
                "    {}.{} ({}): captured={:#X}B serialized={:#X}B first_diff_at={:?}",
                m.package,
                m.export_name,
                m.class_name,
                m.captured_len,
                m.serialized_len,
                m.first_diff_at,
            );
        }
    }
}

/// Walks every loaded `Struct` subtype and compares
/// `serialize_expr(parsed_script)` against the verbatim source bytes
/// that were consumed for that struct's script section.
///
/// Use this after a dump pass to surface where the parsed-tree-driven
/// re-emit would diverge from the original on-disk bytes. Mismatches are
/// expected today wherever `handle_optional_debug_info`'s peek-and-rewind
/// fires (the source advances; the tree may not record those bytes).
/// We accept those, but we don't want any OTHER mismatches.
pub fn script_roundtrip_stats<E: ByteOrder>(
    linkers: &HashMap<String, Rc<RefCell<Linker>>>,
) -> ScriptRoundtripStats {
    let mut stats = ScriptRoundtripStats::default();

    for (pkg_name, linker_rc) in linkers {
        let linker = linker_rc.borrow();
        for (export_index, obj) in linker.objects.iter() {
            let obj_inner = obj.borrow();
            if !obj_inner.is_a(UObjectKind::Struct) {
                continue;
            }
            let s = obj_inner
                .parent_of_kind(UObjectKind::Struct)
                .expect("is_a(Struct) but no parent_of_kind(Struct)")
                .as_any()
                .downcast_ref::<Struct>()
                .expect("parent_of_kind(Struct) downcast failed");
            if s.script_capture.is_empty() {
                continue;
            }
            stats.structs_with_script += 1;

            let mut buf = Vec::with_capacity(s.script_capture.len());
            if script::serialize_expr::<_, E>(&s.parsed_script, &mut buf).is_err() {
                stats.mismatches.push(ScriptRoundtripMismatch {
                    package: pkg_name.clone(),
                    export_name: format_export_name(&linker, *export_index),
                    class_name: format_class_name(&linker, *export_index),
                    captured_len: s.script_capture.len(),
                    serialized_len: buf.len(),
                    first_diff_at: Some(0),
                });
                continue;
            }

            if buf == s.script_capture {
                stats.matching += 1;
            } else {
                let first_diff = buf
                    .iter()
                    .zip(s.script_capture.iter())
                    .position(|(a, b)| a != b)
                    .or_else(|| {
                        if buf.len() != s.script_capture.len() {
                            Some(buf.len().min(s.script_capture.len()))
                        } else {
                            None
                        }
                    });
                if std::env::var("UNREALIN_DUMP_DIFF").is_ok() && stats.mismatches.is_empty() {
                    eprintln!(
                        "=== DIFF DUMP for {} (cap={:#X}, ser={:#X}, diff_at={:?}) ===",
                        format_export_name(&linker, *export_index),
                        s.script_capture.len(),
                        buf.len(),
                        first_diff,
                    );
                    eprintln!("  captured:  {:02x?}", s.script_capture);
                    eprintln!("  canonical: {:02x?}", buf);
                }
                stats.mismatches.push(ScriptRoundtripMismatch {
                    package: pkg_name.clone(),
                    export_name: format_export_name(&linker, *export_index),
                    class_name: format_class_name(&linker, *export_index),
                    captured_len: s.script_capture.len(),
                    serialized_len: buf.len(),
                    first_diff_at: first_diff,
                });
            }
        }
    }

    stats
}

fn format_export_name(linker: &Linker, idx: crate::de::ExportIndex) -> String {
    linker
        .find_export_by_index(idx)
        .map(|e| e.full_name(linker))
        .unwrap_or_else(|| format!("<idx {}>", idx.raw()))
}

fn format_class_name(linker: &Linker, idx: crate::de::ExportIndex) -> String {
    linker
        .find_export_by_index(idx)
        .map(|e| e.class_name(linker).to_string())
        .unwrap_or_else(|| "<unknown>".to_string())
}
