//! Per-package serializer. Splits a loaded `LinearFileDecoder` back into
//! its natural .u/.utx files. Body bytes come from each export's
//! `SerializeUnrealObject::serialize` (default: captured raw stream;
//! per-class override: e.g. `Struct`'s canonical-script splice).
//!
//! ## Corrections
//!
//! Placeholder offsets are written as fixed-width zeros (or the padded
//! packed_int sentinel) during the forward pass, then patched at the
//! end via the [`Correction`] list. This keeps the forward pass linear
//! (no mid-write seeks) and unifies header-table offsets, per-export
//! `serial_offset`, and any future per-type position-dependent fields
//! under a single late-binding mechanism.

use std::io::{self, Seek, SeekFrom, Write};

use byteorder::{ByteOrder, LittleEndian, WriteBytesExt};

use crate::PKG_TAG;
use crate::de::{ExportIndex, GenerationInfo, Linker};
use crate::object::{BodyKind, BodyOffsetPatch, serialize_object};

pub(crate) fn write_packed_int<W: Write>(w: &mut W, value: i32) -> io::Result<()> {
    let sign = if value < 0 { 0x80u8 } else { 0 };
    let mut v: u32 = value.unsigned_abs();
    let mut b0 = (v & 0x3f) as u8;
    if v >= 0x40 {
        b0 |= 0x40;
    }
    b0 |= sign;
    w.write_u8(b0)?;
    if (b0 & 0x40) != 0 {
        v >>= 6;
        loop {
            let mut b = (v & 0x7f) as u8;
            v >>= 7;
            if v != 0 {
                b |= 0x80;
                w.write_u8(b)?;
            } else {
                w.write_u8(b)?;
                break;
            }
        }
    }
    Ok(())
}

/// Number of bytes [`write_packed_int`] would emit for `value`. Mirrors
/// the encoding loop above: 1 byte for `|value| < 0x40`, then +1 byte
/// per 7 bits of magnitude beyond that. Caps at 5 bytes (33 bits of
/// magnitude — beyond what an `i32` can encode anyway).
pub(crate) fn packed_int_size(value: i32) -> usize {
    let mut v: u32 = value.unsigned_abs();
    if v < 0x40 {
        return 1;
    }
    v >>= 6;
    let mut n = 2; // first byte plus the first continuation byte
    while v >= 0x80 {
        v >>= 7;
        n += 1;
    }
    n
}

fn write_string<W: Write>(w: &mut W, s: &str) -> io::Result<()> {
    if s.is_empty() {
        write_packed_int(w, 0)?;
        return Ok(());
    }
    let bytes = s.as_bytes();
    write_packed_int(w, (bytes.len() + 1) as i32)?;
    w.write_all(bytes)?;
    w.write_u8(0)?;
    Ok(())
}

fn write_padded_packed_int<W: Write>(w: &mut W, value: i32) -> io::Result<()> {
    let sign = if value < 0 { 0x80u8 } else { 0 };
    let mut v = value.unsigned_abs();
    let b0 = (v & 0x3f) as u8 | 0x40 | sign;
    v >>= 6;
    w.write_u8(b0)?;
    for _ in 0..3 {
        let b = (v & 0x7f) as u8 | 0x80;
        v >>= 7;
        w.write_u8(b)?;
    }
    let last = (v & 0x7f) as u8;
    w.write_u8(last)?;
    Ok(())
}

/// Encoding to use when applying a [`Correction`].
#[derive(Debug, Copy, Clone)]
pub enum CorrectionEncoding {
    /// Plain little-endian u32 (4 bytes). Used for header-table offsets.
    U32Le,
    /// Fixed 5-byte padded packed_int. Used for the export-table
    /// `serial_offset` field, where the placeholder must be a fixed
    /// width but the value is a packed_int on disk.
    PaddedPackedInt,
}

/// A pending overwrite of a placeholder field. Pushed during the
/// forward write pass at the moment the placeholder is emitted; the
/// resolved value is filled in later (typically once enough has been
/// written to know the absolute file offset of the target). Applied
/// in a single seek-and-write pass at the end of `serialize_linker`.
#[derive(Debug, Copy, Clone)]
pub struct Correction {
    /// Absolute file offset of the placeholder bytes.
    pub at: u64,
    /// Value to write.
    pub value: u32,
    pub encoding: CorrectionEncoding,
}

fn apply_corrections<W, E>(writer: &mut W, corrections: &[Correction]) -> io::Result<()>
where
    W: Write + Seek,
    E: ByteOrder,
{
    for c in corrections {
        writer.seek(SeekFrom::Start(c.at))?;
        match c.encoding {
            CorrectionEncoding::U32Le => writer.write_u32::<E>(c.value)?,
            CorrectionEncoding::PaddedPackedInt => {
                write_padded_packed_int(writer, c.value as i32)?
            }
        }
    }
    Ok(())
}

pub fn serialize_linker<W: Write + Seek, E: ByteOrder>(
    linker: &Linker,
    mut writer: W,
) -> io::Result<()> {
    let pkg = &linker.package;
    let header = &pkg.header;

    let mut corrections: Vec<Correction> = Vec::new();

    // Find a "None" name index in the package's name table for use as
    // the placeholder name on never-loaded exports. SC's runtime
    // `SavePackage` neutralises export entries it never preloaded —
    // their `class_index`, `super_index`, `package_index`, `object_flags`
    // are all zeroed and `object_name` points to "None". We mirror that
    // (see export-table write below). Most SC packages already have
    // "None" in their name table; if a package somehow lacks it, append
    // one and bump the name count by 1.
    let existing_none_idx = pkg.names.iter().position(|n| n.name.eq_ignore_ascii_case("None"));
    let appended_none = existing_none_idx.is_none();
    let none_idx: i32 = match existing_none_idx {
        Some(i) => i as i32,
        None => pkg.names.len() as i32,
    };
    let total_name_count = if appended_none {
        pkg.names.len() as u32 + 1
    } else {
        pkg.names.len() as u32
    };

    writer.write_u32::<E>(PKG_TAG)?;
    writer.write_u32::<E>(header.version)?;
    writer.write_u32::<E>(header.flags)?;

    writer.write_u32::<E>(total_name_count)?;
    let name_offset_pos = writer.stream_position()?;
    writer.write_u32::<E>(0)?;

    writer.write_u32::<E>(header.export_count)?;
    let export_offset_pos = writer.stream_position()?;
    writer.write_u32::<E>(0)?;

    writer.write_u32::<E>(header.import_count)?;
    let import_offset_pos = writer.stream_position()?;
    writer.write_u32::<E>(0)?;

    writer.write_u32::<E>(header.unk)?;
    write_packed_int(&mut writer, header.unknown_data.len() as i32)?;
    writer.write_all(&header.unknown_data)?;

    writer.write_u32::<E>(header.guid_a)?;
    writer.write_u32::<E>(header.guid_b)?;
    writer.write_u32::<E>(header.guid_c)?;
    writer.write_u32::<E>(header.guid_d)?;

    writer.write_u32::<E>(header.generations.len() as u32)?;
    for GenerationInfo {
        export_count,
        name_count: _,
    } in &header.generations
    {
        writer.write_u32::<E>(*export_count)?;
        writer.write_u32::<E>(total_name_count)?;
    }

    let names_pos = writer.stream_position()? as u32;
    corrections.push(Correction {
        at: name_offset_pos,
        value: names_pos,
        encoding: CorrectionEncoding::U32Le,
    });
    for name in &pkg.names {
        write_string(&mut writer, &name.name)?;
        writer.write_u32::<E>(name.flags)?;
    }
    if appended_none {
        write_string(&mut writer, "None")?;
        writer.write_u32::<E>(0)?;
    }

    let imports_pos = writer.stream_position()? as u32;
    corrections.push(Correction {
        at: import_offset_pos,
        value: imports_pos,
        encoding: CorrectionEncoding::U32Le,
    });
    for import in &pkg.imports {
        write_packed_int(&mut writer, import.class_package)?;
        write_packed_int(&mut writer, import.class_name)?;
        writer.write_i32::<E>(import.package_index)?;
        write_packed_int(&mut writer, import.object_name)?;
    }

    // Pre-compute the body bytes (and any per-body patches) for each
    // export by dispatching through each constructed object's
    // `SerializeUnrealObject::serialize`. Default impls return the
    // captured raw stream verbatim (Opaque); per-class overrides
    // substitute canonical bytes (Reconstructed) or attach patches
    // for absolute file offsets baked into the body (Patched).
    struct PendingBody {
        bytes: Vec<u8>,
        patches: Vec<BodyOffsetPatch>,
    }
    let mut bodies: Vec<PendingBody> = Vec::with_capacity(pkg.exports.len());
    for i in 0..pkg.exports.len() {
        let captured: &[u8] = linker
            .captured
            .bodies
            .get(&i)
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        if captured.is_empty() {
            bodies.push(PendingBody {
                bytes: Vec::new(),
                patches: Vec::new(),
            });
            continue;
        }
        let export_index = ExportIndex(i);
        let kind = match linker.objects.get(&export_index) {
            Some(obj) => {
                let inner = obj.borrow();
                serialize_object::<E>(&*inner, linker, export_index, captured)
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?
            }
            None => BodyKind::Opaque(captured.to_vec()),
        };
        let (bytes, patches) = match kind {
            BodyKind::Opaque(b) | BodyKind::Reconstructed(b) => (b, Vec::new()),
            BodyKind::Patched { bytes, patches } => (bytes, patches),
        };
        bodies.push(PendingBody { bytes, patches });
    }

    let effective_size: Vec<i32> = bodies.iter().map(|b| b.bytes.len() as i32).collect();

    // Standard UE2 layout (what UPK Explorer / Unreal-Library expect)
    // is: header → names → imports → exports table → body data, with
    // each `serial_offset` pointing forward into the body region.
    // The challenge is that `serial_offset` is a packed_int whose width
    // depends on its value — but the value depends on the table's size,
    // which depends on the widths. We resolve the chicken-and-egg with
    // a fixed-point iteration: assume every `serial_offset` takes 4
    // bytes initially, compute resulting offsets, observe the actual
    // packed_int width each one needs, and repeat until the width
    // assumption stabilises. Two iterations is enough for typical
    // SC packages (offsets well under 2 GB so width tops out at 4).
    let exports_pos = writer.stream_position()? as u32;
    corrections.push(Correction {
        at: export_offset_pos,
        value: exports_pos,
        encoding: CorrectionEncoding::U32Le,
    });

    // Per-entry size of everything in the export table EXCEPT the
    // serial_offset. Combined with each entry's serial_offset width
    // this gives the total table size. For never-loaded exports
    // (effective_size == 0), mirror SC's runtime `SavePackage` neutral
    // form (class/super/package/flags zeroed, object_name = "None")
    // so size accounting matches the bytes we'll actually emit.
    let mut entry_fixed_sizes: Vec<usize> = Vec::with_capacity(pkg.exports.len());
    for (i, export) in pkg.exports.iter().enumerate() {
        let (ci, si, on) = if effective_size[i] == 0 {
            (0i32, 0i32, none_idx)
        } else {
            (export.class_index, export.super_index, export.object_name)
        };
        let mut sz = packed_int_size(ci);
        sz += packed_int_size(si);
        sz += 4; // package_index (raw u32) — neutralised to 0 below for empties
        sz += packed_int_size(on);
        sz += 4; // object_flags (raw u32) — neutralised to 0 below for empties
        sz += packed_int_size(effective_size[i]); // serial_size
        entry_fixed_sizes.push(sz);
    }

    let mut serial_offset_widths: Vec<usize> = vec![4; pkg.exports.len()];
    let mut serial_offsets: Vec<u32> = vec![0; pkg.exports.len()];
    for _attempt in 0..6 {
        let table_size: usize = (0..pkg.exports.len())
            .map(|i| {
                entry_fixed_sizes[i]
                    + if effective_size[i] > 0 {
                        serial_offset_widths[i]
                    } else {
                        0
                    }
            })
            .sum();
        let body_region_start = exports_pos as u64 + table_size as u64;
        let mut cur = body_region_start;
        for i in 0..pkg.exports.len() {
            if effective_size[i] > 0 {
                serial_offsets[i] = cur as u32;
                cur += effective_size[i] as u64;
            } else {
                serial_offsets[i] = 0;
            }
        }
        let new_widths: Vec<usize> = serial_offsets
            .iter()
            .map(|o| packed_int_size(*o as i32))
            .collect();
        if new_widths == serial_offset_widths {
            break;
        }
        serial_offset_widths = new_widths;
    }

    // Now write the export table with the resolved widths. `write_packed_int`
    // produces the same width we just budgeted for, so the bodies that
    // follow land at exactly the offsets we've stamped in. Never-loaded
    // exports (effective_size == 0) get the runtime-`SavePackage` neutral
    // form: class/super/package/flags zeroed, object_name = "None". This
    // matches the on-disk shape produced by SC's `LoadMap`-end SavePackage
    // hook, and lets natural readers (UPK Explorer's UClass adapter is
    // trivial for empty bodies; its Texture adapter is not) skip them
    // without tripping `EndOffset = -1` bounds checks on absent data.
    for (i, export) in pkg.exports.iter().enumerate() {
        let (ci, si, pi, on, ofl) = if effective_size[i] == 0 {
            (0i32, 0i32, 0i32, none_idx, 0u32)
        } else {
            (export.class_index, export.super_index, export.package_index, export.object_name, export.object_flags)
        };
        write_packed_int(&mut writer, ci)?;
        write_packed_int(&mut writer, si)?;
        writer.write_i32::<E>(pi)?;
        write_packed_int(&mut writer, on)?;
        writer.write_u32::<E>(ofl)?;
        write_packed_int(&mut writer, effective_size[i])?;
        if effective_size[i] > 0 {
            let pre = writer.stream_position()?;
            write_packed_int(&mut writer, serial_offsets[i] as i32)?;
            let actual_w = (writer.stream_position()? - pre) as usize;
            debug_assert_eq!(
                actual_w, serial_offset_widths[i],
                "serial_offset width mismatch for export {} (actual {}, planned {})",
                i, actual_w, serial_offset_widths[i]
            );
        }
    }

    // Write bodies, applying per-body BodyOffsetPatches now that each
    // body's absolute file position is known (e.g. UTexture mipmap
    // `SeekPos` fields baked into the captured bytes).
    for (i, body) in bodies.iter_mut().enumerate() {
        if body.bytes.is_empty() {
            continue;
        }
        let body_pos = writer.stream_position()? as u32;
        debug_assert_eq!(
            body_pos, serial_offsets[i],
            "body {i} placed at {body_pos:#X} but table says {:#X}",
            serial_offsets[i]
        );
        for patch in &body.patches {
            let new_value = body_pos + patch.target_offset_within_body as u32;
            let bytes_le = new_value.to_le_bytes();
            if patch.body_offset + 4 <= body.bytes.len() {
                body.bytes[patch.body_offset..patch.body_offset + 4]
                    .copy_from_slice(&bytes_le);
            }
        }
        writer.write_all(&body.bytes)?;
    }

    apply_corrections::<_, E>(&mut writer, &corrections)?;

    Ok(())
}

pub fn serialize_linker_le<W: Write + Seek>(linker: &Linker, writer: W) -> io::Result<()> {
    serialize_linker::<_, LittleEndian>(linker, writer)
}
