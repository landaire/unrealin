use byteorder::*;
use std::io::{self, Seek};
use std::io::{SeekFrom, Write};
use winnow::BStr;

use crate::{PKG_TAG, de::*};

fn write_packed_int<W: Write>(writer: &mut W, value: i32) -> io::Result<()> {
    let sign = if value < 0 { 0x80 } else { 0x00 };
    let mut v: u32 = value.unsigned_abs(); // handles i32::MIN safely (becomes 2147483648)

    // B0 carries 6 bits of payload, plus sign and "more" flag if needed.
    let mut b0 = (v & 0x3f) as u8;
    if v >= 0x40 {
        b0 |= 0x40; // more bytes follow
    }
    b0 |= sign;
    writer.write_u8(b0)?;

    if (b0 & 0x40) != 0 {
        // Emit remaining bits in 7-bit chunks, MSB=1 while more chunks remain.
        v >>= 6;
        loop {
            let mut b = (v & 0x7f) as u8;
            v >>= 7;
            if v != 0 {
                b |= 0x80; // continuation
                writer.write_u8(b)?;
            } else {
                writer.write_u8(b)?; // final chunk (no continuation bit)
                break;
            }
        }
    }

    Ok(())
}

fn write_var_string<W: Write>(writer: &mut W, value: &BStr) -> io::Result<()> {
    if value.is_empty() {
        writer.write_u8(0)?;
        return Ok(());
    }
    write_packed_int(writer, (value.len() + 1) as i32)?;
    writer.write_all(value)?;

    Ok(())
}

pub fn serialize_unreal_package<W: Write + Seek>(
    mut writer: W,
    package: &UnrealPackage<'_>,
) -> io::Result<()> {
    let RawPackage {
        header,
        names,
        imports,
        exports,
    } = &package.raw_package;

    let PackageHeader {
        version,
        flags,
        name_count,
        name_offset,
        export_count,
        export_offset,
        import_count,
        import_offset,
        unk,
        unknown_data,
        guid_a,
        guid_b,
        guid_c,
        guid_d,
        generations,
    } = header;

    struct Correction {
        offset: u64,
        value: u32,
    }

    let mut offset_corrections = Vec::new();

    writer.write_u32::<LE>(PKG_TAG)?;
    writer.write_u32::<LE>(*version)?;
    writer.write_u32::<LE>(*flags)?;

    writer.write_u32::<LE>(*name_count)?;
    let name_offset_offset = writer.stream_position()?;
    writer.write_u32::<LE>(*name_offset)?;

    writer.write_u32::<LE>(*export_count)?;
    let export_offset_offset = writer.stream_position()?;
    writer.write_u32::<LE>(*export_offset)?;

    writer.write_u32::<LE>(*import_count)?;
    let import_offset_offset = writer.stream_position()?;
    writer.write_u32::<LE>(*import_offset)?;

    writer.write_u32::<LE>(*unk)?;
    write_packed_int(&mut writer, unknown_data.len() as i32)?;

    writer.write_all(unknown_data)?;

    writer.write_u32::<LE>(*guid_a)?;
    writer.write_u32::<LE>(*guid_b)?;
    writer.write_u32::<LE>(*guid_c)?;
    writer.write_u32::<LE>(*guid_d)?;

    writer.write_u32::<LE>(generations.len() as u32)?;

    for GenerationInfo {
        export_count,
        name_count,
    } in generations
    {
        writer.write_u32::<LE>(*export_count)?;
        writer.write_u32::<LE>(*name_count)?;
    }

    let names_offset = writer.stream_position()?;

    offset_corrections.push(Correction {
        offset: name_offset_offset,
        value: names_offset as u32,
    });

    // Write out the name table
    for Name { name, flags } in names {
        write_var_string(&mut writer, name)?;
        writer.write_u32::<LE>(*flags)?;
    }

    let imports_position = writer.stream_position()?;
    offset_corrections.push(Correction {
        offset: import_offset_offset,
        value: imports_position as u32,
    });
    for Import {
        class_package,
        class_name,
        package_index,
        object_name,
    } in imports
    {
        write_packed_int(&mut writer, *class_package)?;
        write_packed_int(&mut writer, *class_name)?;
        writer.write_i32::<LE>(*package_index)?;
        write_packed_int(&mut writer, *object_name)?;
    }

    let exports_position = writer.stream_position()?;
    offset_corrections.push(Correction {
        offset: export_offset_offset,
        value: exports_position as u32,
    });

    let mut object_export_fixups = Vec::with_capacity(exports.len());
    for ObjectExport {
        class_index,
        super_index,
        package_index,
        object_name,
        object_flags,
        serial_size,
        serial_offset,
        data,
    } in exports
    {
        write_packed_int(&mut writer, *class_index)?;
        write_packed_int(&mut writer, *super_index)?;
        writer.write_i32::<LE>(*package_index)?;

        write_packed_int(&mut writer, *object_name)?;

        writer.write_u32::<LE>(*object_flags)?;

        write_packed_int(&mut writer, *serial_size)?;
        if *serial_size > 0 {
            object_export_fixups.push(Some(writer.stream_position()?));
            write_packed_int(&mut writer, *serial_offset)?;
        } else {
            object_export_fixups.push(None);
        }
    }

    for (i, export) in exports.iter().enumerate() {
        if export.serial_size == 0 {
            continue;
        }

        let offset = writer.stream_position()?;
        writer.write_all(export.data.expect("export has no data associated?"))?;

        offset_corrections.push(Correction {
            offset: object_export_fixups[i].expect("no fixup?"),
            value: offset as u32,
        });
    }

    for correction in offset_corrections {
        writer.seek(SeekFrom::Start(correction.offset))?;
        writer.write_u32::<LE>(correction.value)?;
    }

    Ok(())
}
