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
    writer.write_u8(0x0)?;

    Ok(())
}

pub fn serialize_unreal_package<W: Write + Seek>(
    mut writer: W,
    package: &mut RawPackage<'_>,
) -> io::Result<()> {
    let RawPackage {
        header,
        names,
        imports,
        exports,
    } = package;

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
        packed: bool,
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
        packed: false,
    });

    // Write out the name table
    for Name { name, flags } in names.iter() {
        write_var_string(&mut writer, name)?;
        writer.write_u32::<LE>(*flags)?;
    }

    let imports_position = writer.stream_position()?;
    offset_corrections.push(Correction {
        offset: import_offset_offset,
        value: imports_position as u32,
        packed: false,
    });
    for Import {
        class_package,
        class_name,
        package_index,
        object_name,
        object,
    } in imports.iter()
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
        packed: false,
    });

    for (
        i,
        ObjectExport {
            class_index,
            super_index,
            package_index,
            object_name,
            object_flags,
            serial_size,
            serial_offset,
            data,
        },
    ) in exports.iter_mut().enumerate()
    {
        write_packed_int(&mut writer, *class_index)?;

        write_packed_int(&mut writer, *super_index)?;

        writer.write_i32::<LE>(*package_index)?;

        write_packed_int(&mut writer, *object_name)?;

        writer.write_u32::<LE>(*object_flags)?;

        let new_serial_size = data.iter().fold(0, |accum, (_offset, data)| accum + data.len());
        println!("Export index: {i:#X}. Old size={serial_size:#X}, new size={new_serial_size:#X}");
        *serial_size = new_serial_size as i32;

        write_packed_int(&mut writer, *serial_size)?;

        if *serial_size > 0 {
            // Write out a fix-sized placeholder
            writer.write_all([0x0, 0x0, 0x0, 0x0, 0x0].as_slice())?;
        }
    }

    for export in exports.iter_mut() {
        let new_serial_size = export.data.iter().fold(0, |accum, (offset, data)| accum + data.len());
        if new_serial_size == 0 {
            continue;
        }

        let offset_before = export.serial_offset;
        export.serial_offset = writer.stream_position()? as i32;
        let mut normalized_offset = 0u32;
        for (data_idx, (offset, data)) in export.data.iter().enumerate() {
            normalized_offset += data.len() as u32;

            let class_name = if export.class_index < 0 {
                let idx = (-export.class_index) as usize - 1;
                names[imports[idx].object_name as usize].name
            } else  {
                BStr::new(b"Class".as_slice())
            };

            if class_name == "Texture" {
                if offset_before == 0x4431C0{
                    println!("BLOCK START");
                }

                let mut ranges: Vec<(u32, std::ops::Range<usize>)> = Vec::new();
                if let Some((next_offset, next_data)) = export.data.get(data_idx + 1) {
                    let normalized_next_offset = normalized_offset + next_data.len() as u32;

                    let next_offset = (*next_offset + next_data.len() as u64) as u32;
                    let next_offset_bytes = next_offset.to_le_bytes();

                    for (i, window) in data.windows(4).enumerate() {
                        // if offset_before == 0x4431C0  {
                        //     println!("{window:X?}, {next_offset_bytes:X?}");
                        // }
                        if window == next_offset_bytes {
                            if let Some((last_off, last_range)) = ranges.last() {
                                let last_range_end = last_range.end + 4;
                                ranges.push((normalized_next_offset, (last_range_end)..(last_range_end + i)));
                            } else {
                                ranges.push((normalized_next_offset, 0..i));
                            }
                        }
                    }
                }

                if ranges.is_empty() {
                    writer.write_all(data)?;
                } else {
                    for (next_offset, range) in ranges.iter().cloned() {
                        writer.write_all(&data[range])?;

                        // Write out zero so it skips zero bytes
                        // TODO: I tried putting in the offset here but it broke things...
                        // so leaving this as zeroes for now
                        writer.write_all(&[0, 0, 0, 0])?;
                    }
                    // Write out the final bit of data
                    let final_range_start = ranges.last().unwrap().1.end + 4;
                    let final_range = final_range_start..data.len();
                    writer.write_all(&data[final_range])?;
                }
            } else {
                writer.write_all(data)?;
            }
        }
    }

    writer.seek(SeekFrom::Start(exports_position))?;

    // Go update the exports table
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
            write_packed_int(&mut writer, *serial_offset)?;
        }
    }

    for correction in offset_corrections {
        writer.seek(SeekFrom::Start(correction.offset))?;
        if correction.packed {
            write_packed_int(&mut writer, correction.value as i32)?;
        } else {
            writer.write_u32::<LE>(correction.value)?;
        }
    }

    Ok(())
}
