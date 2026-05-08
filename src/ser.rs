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
use crate::object::serialize_object;

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

    writer.write_u32::<E>(PKG_TAG)?;
    writer.write_u32::<E>(header.version)?;
    writer.write_u32::<E>(header.flags)?;

    writer.write_u32::<E>(header.name_count)?;
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
        name_count,
    } in &header.generations
    {
        writer.write_u32::<E>(*export_count)?;
        writer.write_u32::<E>(*name_count)?;
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

    // Pre-compute the body bytes for each export by dispatching through
    // each constructed object's `SerializeUnrealObject::serialize`.
    // Default impls return the captured raw stream verbatim; per-class
    // overrides (e.g. `Struct`'s script-section splice) substitute
    // canonical bytes. We need every body's length up-front so the
    // export table's serial_size is correct before the bodies are
    // written.
    let mut bodies: Vec<Vec<u8>> = Vec::with_capacity(pkg.exports.len());
    for i in 0..pkg.exports.len() {
        let captured: &[u8] = linker
            .captured
            .bodies
            .get(&i)
            .map(|v| v.as_slice())
            .unwrap_or(&[]);
        if captured.is_empty() {
            bodies.push(Vec::new());
            continue;
        }
        let export_index = ExportIndex(i);
        let body = match linker.objects.get(&export_index) {
            Some(obj) => {
                let inner = obj.borrow();
                let kind = serialize_object::<E>(&*inner, linker, export_index, captured)
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
                kind.into_bytes()
            }
            None => captured.to_vec(),
        };
        bodies.push(body);
    }

    let effective_size: Vec<i32> = bodies.iter().map(|b| b.len() as i32).collect();

    let exports_pos = writer.stream_position()? as u32;
    corrections.push(Correction {
        at: export_offset_pos,
        value: exports_pos,
        encoding: CorrectionEncoding::U32Le,
    });
    let mut export_serial_offset_pos: Vec<u64> = Vec::with_capacity(pkg.exports.len());
    for (i, export) in pkg.exports.iter().enumerate() {
        write_packed_int(&mut writer, export.class_index)?;
        write_packed_int(&mut writer, export.super_index)?;
        writer.write_i32::<E>(export.package_index)?;
        write_packed_int(&mut writer, export.object_name)?;
        writer.write_u32::<E>(export.object_flags)?;
        write_packed_int(&mut writer, effective_size[i])?;
        let offset_field_pos = writer.stream_position()?;
        if effective_size[i] > 0 {
            writer.write_all(&[0xC0, 0x80, 0x80, 0x80, 0x00])?;
        }
        export_serial_offset_pos.push(offset_field_pos);
    }

    for (i, body) in bodies.iter().enumerate() {
        if body.is_empty() {
            continue;
        }
        let body_pos = writer.stream_position()? as u32;
        writer.write_all(body)?;
        corrections.push(Correction {
            at: export_serial_offset_pos[i],
            value: body_pos,
            encoding: CorrectionEncoding::PaddedPackedInt,
        });
    }

    apply_corrections::<_, E>(&mut writer, &corrections)?;

    Ok(())
}

pub fn serialize_linker_le<W: Write + Seek>(linker: &Linker, writer: W) -> io::Result<()> {
    serialize_linker::<_, LittleEndian>(linker, writer)
}
