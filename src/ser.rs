//! Per-package serializer. Splits a loaded `LinearFileDecoder` back into
//! its natural .u/.utx files. Body bytes come from the captured frames
//! the deserializer recorded for each export, so packed_int FName and
//! object indices remain valid in the output without rewriting.

use std::io::{self, Seek, SeekFrom, Write};

use byteorder::{ByteOrder, LittleEndian, WriteBytesExt};

use crate::PKG_TAG;
use crate::de::{GenerationInfo, Linker};

fn write_packed_int<W: Write>(w: &mut W, value: i32) -> io::Result<()> {
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

// Always emit serial_offset as a fixed 5-byte packed_int so we can patch
// it in place without rewriting the rest of the export table.
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

pub fn serialize_linker<W: Write + Seek, E: ByteOrder>(
    linker: &Linker,
    mut writer: W,
) -> io::Result<()> {
    let pkg = &linker.package;
    let header = &pkg.header;

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
    for name in &pkg.names {
        write_string(&mut writer, &name.name)?;
        writer.write_u32::<E>(name.flags)?;
    }

    let imports_pos = writer.stream_position()? as u32;
    for import in &pkg.imports {
        write_packed_int(&mut writer, import.class_package)?;
        write_packed_int(&mut writer, import.class_name)?;
        writer.write_i32::<E>(import.package_index)?;
        write_packed_int(&mut writer, import.object_name)?;
    }

    let effective_size: Vec<i32> = pkg
        .exports
        .iter()
        .enumerate()
        .map(|(i, _)| {
            linker
                .captured
                .bodies
                .get(&i)
                .map(|b| b.len() as i32)
                .unwrap_or(0)
        })
        .collect();

    let exports_pos = writer.stream_position()? as u32;
    let mut export_table_entries: Vec<u64> = Vec::with_capacity(pkg.exports.len());
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
        export_table_entries.push(offset_field_pos);
    }

    let mut new_serial_offsets: Vec<u32> = Vec::with_capacity(pkg.exports.len());
    for (i, _export) in pkg.exports.iter().enumerate() {
        if effective_size[i] <= 0 {
            new_serial_offsets.push(0);
            continue;
        }
        let body_pos = writer.stream_position()? as u32;
        let body = &linker.captured.bodies[&i];
        writer.write_all(body)?;
        new_serial_offsets.push(body_pos);
    }

    for (i, offset_field_pos) in export_table_entries.iter().enumerate() {
        if effective_size[i] <= 0 {
            continue;
        }
        writer.seek(SeekFrom::Start(*offset_field_pos))?;
        write_padded_packed_int(&mut writer, new_serial_offsets[i] as i32)?;
    }

    writer.seek(SeekFrom::Start(name_offset_pos))?;
    writer.write_u32::<E>(names_pos)?;
    writer.seek(SeekFrom::Start(import_offset_pos))?;
    writer.write_u32::<E>(imports_pos)?;
    writer.seek(SeekFrom::Start(export_offset_pos))?;
    writer.write_u32::<E>(exports_pos)?;

    Ok(())
}

pub fn extension_for(name: &str) -> &'static str {
    match name.to_ascii_lowercase().as_str() {
        n if n.ends_with(".utx") => "utx",
        n if n.ends_with(".uax") => "uax",
        n if n.ends_with(".unr") => "unr",
        _ => "u",
    }
}

pub fn serialize_linker_le<W: Write + Seek>(linker: &Linker, writer: W) -> io::Result<()> {
    serialize_linker::<_, LittleEndian>(linker, writer)
}
