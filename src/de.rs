use std::{
    collections::{HashMap, VecDeque},
    io::{BufRead, Cursor},
    rc::Rc,
};

use flate2::read::ZlibDecoder;
use serde::Deserialize;
use winnow::{
    BStr, Parser as _,
    binary::{le_i32, le_u32, u8},
    combinator::repeat,
    error::ContextError,
    token::take,
};

use crate::{
    LIN_FILE_TABLE_TAG, PKG_TAG,
    common::{ExportRead, ExportedData},
    object::UnrealObject,
};
use crate::{common::normalize_index, object::RcUnrealObject};

struct Block<'a> {
    uncompressed_len: u32,
    compressed_len: u32,
    compressed_data: &'a [u8],
}

fn read_block<'i>(input: &mut &'i [u8]) -> winnow::Result<Block<'i>> {
    let uncompressed_len = le_u32(input)?;
    let compressed_len = le_u32(input)?;
    let compressed_data = take(compressed_len).parse_next(input)?;

    Ok(Block {
        uncompressed_len,
        compressed_len,
        compressed_data,
    })
}

/// Decodes the packed integer from the byte stream.
/// Assumes `u8(input)` reads one byte from `input`.
pub fn read_packed_int(input: &mut &[u8]) -> winnow::Result<i32> {
    const CONTINUE_BIT: u8 = 0x40;
    const NEGATE_BIT: u8 = 0x80;

    let b0 = u8(input)?;

    // Build up the unsigned magnitude.
    let mut value: u32 = 0;

    if (b0 & CONTINUE_BIT) != 0 {
        let b1 = u8(input)?;
        if (b1 & NEGATE_BIT) != 0 {
            let b2 = u8(input)?;
            if (b2 & NEGATE_BIT) != 0 {
                let b3 = u8(input)?;
                if (b3 & NEGATE_BIT) != 0 {
                    let b4 = u8(input)?;
                    value = b4 as u32;
                }
                value = (value << 7) + ((b3 & (NEGATE_BIT - 1)) as u32);
            }
            value = (value << 7) + ((b2 & (NEGATE_BIT - 1)) as u32);
        }
        value = (value << 7) + ((b1 & (NEGATE_BIT - 1)) as u32);
    }

    value = (value << 6) + ((b0 & (CONTINUE_BIT - 1)) as u32);

    // Apply sign bit from B0.
    let mut result = value as i32;
    if (b0 & 0x80) != 0 {
        result = -result;
    }

    Ok(result)
}

fn decode_compact_index2(input: &mut &[u8]) -> winnow::Result<i32> {
    const CONTINUE_BIT: u8 = 0x40;
    const NEGATE_BIT: u8 = 0x80;
    const MAX_ADDITIONAL_BYTES: usize = 4;

    let b0 = u8(input)?;

    let negative = (b0 & NEGATE_BIT) != 0;
    let mut value: i32 = (b0 & (CONTINUE_BIT - 1)) as i32; // 6 data bits from first byte
    let mut shift = 6;

    let mut byte_count = 1;
    // if continue bit set in first byte, keep pulling 7-bit groups
    if (b0 & CONTINUE_BIT) != 0 {
        for i in 0..MAX_ADDITIONAL_BYTES {
            byte_count += 1;
            let bi = u8(input)?;

            let data = if i < 3 { bi & (CONTINUE_BIT - 1) } else { bi }; // last (5th) byte uses all 8 bits
            value |= (data as i32) << shift;
            shift += 7;

            if i < (MAX_ADDITIONAL_BYTES - 1) && (bi & CONTINUE_BIT) == 0 {
                break;
            }
        }
    }

    let result = if negative { -value } else { value };

    Ok(result)
}

#[derive(Debug)]
pub(crate) struct FileEntry<'i> {
    pub name: &'i BStr,
    pub offset: u32,
    pub len: u32,
    pub unk: u32,
}

fn read_file_entry<'i>(input: &mut &'i [u8]) -> winnow::Result<FileEntry<'i>> {
    let name = read_var_string(input)?;
    let offset = le_u32(input)?;
    let len = le_u32(input)?;
    let unk = le_u32(input)?;

    assert_eq!(unk, 0, "unknown value is not zero");

    let entry = FileEntry {
        name,
        offset,
        len,
        unk,
    };

    Ok(entry)
}

#[derive(Debug)]
pub struct PackageHeader<'i> {
    pub version: u32,
    pub flags: u32,
    pub name_count: u32,
    pub name_offset: u32,
    pub export_count: u32,
    pub export_offset: u32,
    pub import_count: u32,
    pub import_offset: u32,
    pub unk: u32,
    pub unknown_data: &'i [u8],
    pub guid_a: u32,
    pub guid_b: u32,
    pub guid_c: u32,
    pub guid_d: u32,
    pub generations: Vec<GenerationInfo>,
}

fn read_var_string<'i>(input: &mut &'i [u8]) -> winnow::Result<&'i BStr> {
    let name_len = read_packed_int(input)?;
    let name = take((name_len as usize).saturating_sub(1)).parse_next(input)?;
    if name_len > 0 {
        let _null_term = u8(input)?;
    }

    Ok(BStr::new(name))
}

#[derive(Debug)]
pub struct Name<'i> {
    pub name: &'i BStr,
    pub flags: u32,
}

fn read_name<'i>(input: &mut &'i [u8]) -> winnow::Result<Name<'i>> {
    Ok(Name {
        name: read_var_string(input)?,
        flags: le_u32(input)?,
    })
}

#[derive(Debug)]
pub(crate) struct Import {
    pub class_package: i32,
    pub class_name: i32,
    pub package_index: i32,
    pub object_name: i32,
    pub(crate) object: Option<RcUnrealObject>,
}

impl Import {
    pub fn class_name<'i>(&self, package: &'i RawPackage<'_>) -> &'i BStr {
        package.names[self.class_name as usize].name
    }

    pub fn full_name(&self, package: &RawPackage<'_>) -> String {
        format!(
            "{} {}.{}",
            package.names[self.class_name as usize].name,
            package.names[self.class_package as usize].name,
            package.names[self.object_name as usize].name
        )
    }

    pub fn resolve_export<'i>(&self, container: &'i RawPackage<'_>) -> &'i ObjectExport<'i> {
        let normalized_index = normalize_index(self.package_index);
        &container.exports[normalized_index]
    }
}

fn read_import(input: &mut &[u8]) -> winnow::Result<Import> {
    let class_package = read_packed_int(input)?;

    let class_name = read_packed_int(input)?;

    let package_index = le_i32(input)?;

    let object_name = read_packed_int(input)?;

    Ok(Import {
        class_package,
        class_name,
        package_index,
        object_name,
        object: None,
    })
}

#[derive(Debug, Deserialize)]
pub struct ObjectExport<'i> {
    pub class_index: i32,
    pub super_index: i32,
    pub package_index: i32,
    pub object_name: i32,
    pub object_flags: u32,
    pub serial_size: i32,
    pub serial_offset: i32,
    #[serde(skip)]
    pub data: Vec<&'i [u8]>,
}

impl ObjectExport<'_> {
    fn partially_eq(&self, other: &Self) -> bool {
        self.class_index == other.class_index
            && self.super_index == other.super_index
            && self.package_index == other.package_index
            && self.object_flags == other.object_flags
            && self.serial_size == other.serial_size
            && self.serial_offset == other.serial_offset
    }
}

impl ObjectExport<'_> {
    fn object_name<'p>(&self, package: &'p RawPackage<'_>) -> &'p BStr {
        package.names[self.object_name as usize].name
    }

    fn class_name<'p>(&self, package: &'p RawPackage<'_>) -> &'p BStr {
        let normalized_index = normalize_index(self.class_index);

        if normalized_index == 0 {
            return BStr::new(b"Class".as_slice());
        }

        package.names[package.exports[normalized_index].object_name as usize].name
    }
}

fn read_export<'i>(input: &mut &'i [u8]) -> winnow::Result<ObjectExport<'i>> {
    let class_index = read_packed_int(input)?;
    let super_index = read_packed_int(input)?;

    let package_index = le_i32(input)?;

    let object_name = read_packed_int(input)?;

    let object_flags = le_u32(input)?;

    let serial_size = read_packed_int(input)?;

    let serial_offset = if serial_size != 0 {
        read_packed_int(input)?
    } else {
        0
    };
    Ok(ObjectExport {
        class_index,
        super_index,
        package_index,
        object_name,
        object_flags,
        serial_size,
        serial_offset,
        data: Vec::new(),
    })
}

#[derive(Debug)]
pub(crate) struct GenerationInfo {
    pub export_count: u32,
    pub name_count: u32,
}

fn read_generation_info(input: &mut &[u8]) -> winnow::Result<GenerationInfo> {
    let export_count = le_u32(input)?;
    let name_count = le_u32(input)?;

    Ok(GenerationInfo {
        export_count,
        name_count,
    })
}

fn read_file_table<'i>(input: &mut &'i [u8]) -> winnow::Result<Vec<FileEntry<'i>>> {
    // Reset input to skip past most of the header
    let _ = take(0x10_usize).parse_next(input)?;

    let file_entry_count = read_packed_int(input)?;

    let file_table: Vec<FileEntry<'_>> =
        repeat(file_entry_count as usize, read_file_entry).parse_next(input)?;

    Ok(file_table)
}

fn read_package_header<'i>(input: &mut &'i [u8]) -> winnow::Result<PackageHeader<'i>> {
    let version = le_u32(input)?;
    println!("Version: {:#X}", version);
    let flags = le_u32(input)?;
    let name_count = le_u32(input)?;
    println!("name_count: {:#X}", name_count);
    let name_offset = le_u32(input)?;
    let export_count = le_u32(input)?;
    let export_offset = le_u32(input)?;
    let import_count = le_u32(input)?;
    let import_offset = le_u32(input)?;

    let unk = le_u32(input)?;
    println!("Unknown value: {:#X}", unk);

    let unk_data_count = read_packed_int(input)?;
    println!("data count: {:#X}", unk_data_count);

    let unknown_data = take(unk_data_count as usize).parse_next(input)?;

    let guid_a = le_u32(input)?;
    let guid_b = le_u32(input)?;
    let guid_c = le_u32(input)?;
    let guid_d = le_u32(input)?;

    let generation_count = le_u32(input)?;
    let generations: Vec<_> =
        repeat(generation_count as usize, read_generation_info).parse_next(input)?;

    Ok(PackageHeader {
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
    })
}

#[derive(Debug)]
pub struct RawPackage<'i> {
    pub header: PackageHeader<'i>,
    pub names: Vec<Name<'i>>,
    pub imports: Vec<Import>,
    pub exports: Vec<ObjectExport<'i>>,
}

pub fn read_package<'i>(input: &mut &'i [u8]) -> winnow::Result<RawPackage<'i>> {
    let orig_input = *input;
    let len_before = input.len();

    let header = read_package_header(input).expect("failed to read package header");

    let names: Vec<_> = repeat(header.name_count as usize, read_name)
        .parse_next(input)
        .expect("failed to parse names");

    let imports: Vec<_> = repeat(header.import_count as usize, read_import)
        .parse_next(input)
        .expect("failed to parse import");

    println!(
        "Exports start at: {:#X}, {:#X?}",
        orig_input.len() - input.len(),
        &input[..0x10.min(input.len())]
    );
    let exports: Vec<_> = repeat(header.export_count as usize, read_export)
        .parse_next(input)
        .expect("failed to parse export");

    Ok(RawPackage {
        header,
        names,
        imports,
        exports,
    })
}

pub struct LinearFile<'i> {
    pub file_table: Option<Vec<FileEntry<'i>>>,
    pub packages: Vec<RawPackage<'i>>,
}

impl<'i> LinearFile<'i> {
    pub fn file_table(&self) -> Option<&Vec<FileEntry<'i>>> {
        self.file_table.as_ref()
    }

    pub fn packages_mut(&mut self) -> &mut [RawPackage<'i>] {
        &mut self.packages
    }

    pub fn packages(&self) -> &[RawPackage<'i>] {
        &self.packages
    }
}

pub struct UnrealPackage<'i> {
    pub(crate) raw_package: RawPackage<'i>,
    pub(crate) class_types: HashMap<&'i BStr, ()>,
    pub(crate) objects: Vec<Rc<UnrealObject>>,
}

pub fn decompress_linear_file(mut input: &[u8]) -> Vec<u8> {
    let mut out_data = Vec::new();

    // Read the first data block to get the decompressed size
    let uncompressed_data_size = {
        let block = read_block(&mut input).expect("failed to read block");
        let mut reader = ZlibDecoder::new(block.compressed_data);
        let mut bytes = [0u8; 4];
        let mut cursor = Cursor::new(bytes.as_mut_slice());
        std::io::copy(&mut reader, &mut cursor).expect("failed to read zlib data ");

        u32::from_le_bytes(bytes)
    };

    out_data.reserve(uncompressed_data_size as usize);

    let compressed_data_size = {
        let block = read_block(&mut input).expect("failed to read block");
        let mut reader = ZlibDecoder::new(block.compressed_data);
        let mut bytes = [0u8; 4];
        let mut cursor = Cursor::new(bytes.as_mut_slice());
        std::io::copy(&mut reader, &mut cursor).expect("failed to read zlib data");

        u32::from_le_bytes(bytes)
    };

    let unk1 = {
        let block = read_block(&mut input).expect("failed to read block");
        let mut reader = ZlibDecoder::new(block.compressed_data);
        let mut bytes = [0u8; 4];
        let mut cursor = Cursor::new(bytes.as_mut_slice());
        std::io::copy(&mut reader, &mut cursor).expect("failed to read zlib data");

        u32::from_le_bytes(bytes)
    };

    let unk2 = {
        let block = read_block(&mut input).expect("failed to read block");
        let mut reader = ZlibDecoder::new(block.compressed_data);
        let mut bytes = [0u8; 4];
        let mut cursor = Cursor::new(bytes.as_mut_slice());
        std::io::copy(&mut reader, &mut cursor).expect("failed to read zlib data");

        u32::from_le_bytes(bytes)
    };

    println!("uncompressed_data_size: {uncompressed_data_size:#X}");
    println!("compressed_data_size: {compressed_data_size:#X}");
    println!("unk1: {unk1:#X}");
    println!("unk2: {unk2:#X}");

    while !input.is_empty() {
        let block = read_block(&mut input).expect("failed to read block");
        let mut reader = ZlibDecoder::new(block.compressed_data);

        std::io::copy(&mut reader, &mut out_data).expect("failed to read zlib data");
    }

    out_data
}

pub fn decode_linear_file<'i>(common_lin_input: &'i [u8], map_input: &'i [u8]) -> LinearFile<'i> {
    let mut file_table = None;
    let mut raw_packages: Vec<RawPackage<'_>> = Vec::new();

    let mut reader = std::fs::File::open("/var/tmp/reads.json").expect("failed to open reads file");

    let mut metadata: ExportedData = serde_json::from_reader(reader).expect("failed to parse read");
    metadata.file_ptr_order.reverse();
    metadata
        .file_reads
        .iter_mut()
        .for_each(|(_k, v)| v.reverse());
    metadata.file_load_order.reverse();

    let mut offsets = metadata.file_reads;

    for mut input in [common_lin_input, map_input] {
        let orig_input = input;

        let unk = le_u32::<_, ContextError>(&mut input).expect("failed to parse tag");

        println!("{:#X}", unk);

        let name = read_var_string(&mut input).expect("failed to read lin name");
        println!("{}", name);

        let mut current_file = metadata.file_ptr_order.pop().unwrap();
        println!("offsets count: {}", offsets.len());

        'parser_loop: while input.len() > 4 {
            let input_before = input;
            let tag = le_u32::<_, ContextError>(&mut input).expect("failed to parse tag");
            // println!("Processing at {:#02X?}", &input[..16]);

            match tag {
                LIN_FILE_TABLE_TAG => {
                    file_table =
                        Some(read_file_table(&mut input).expect("failed to read file table"));
                    println!(
                        "File table length: {:#X}",
                        file_table.as_ref().map(|t| t.len()).unwrap_or_default()
                    );
                    println!("{file_table:#X?}");
                    continue;
                }
                PKG_TAG => {
                    let package = read_package(&mut input).expect("failed to read package");
                    println!("{:#X?}", &package.header);

                    println!("Import count: {}", package.imports.len());
                    println!("Imports:");
                    for i in &package.imports {
                        //println!("{}", i.full_name(&package));
                        println!("{:X?}", i);
                    }

                    println!(
                        "Export size: {:#X}",
                        package
                            .exports
                            .iter()
                            .fold(0, |accum, e| accum + e.serial_size)
                    );
                    println!("All exports");

                    println!("Export count: {}", package.exports.len());
                    println!("Export:");
                    for (i, export) in package.exports.iter().enumerate() {
                        println!("({i:#X}) {:#X?}", export);
                        println!("\t{}", export.object_name(&package));
                        //println!("\t{}", export.class_name(&package));
                    }

                    println!("End of export table {:#X}", orig_input.len() - input.len());

                    raw_packages.push(package);
                }
                tag => {
                    let current_offset = orig_input.len() - input_before.len();
                    // input = &input_before[1..];
                    input = input_before;
                    // continue;

                    // println!("Unexpected tag at: {:#X}", orig_input.len() - input.len());

                    let Some(read_info) = offsets.get_mut(&current_file).unwrap().pop() else {
                        break;
                    };

                    for raw_package in &mut raw_packages {
                        for export in &mut raw_package.exports {
                            if export.partially_eq(&read_info.export) {
                                println!("Reading {:#X} bytes", read_info.len);
                                let data = take::<_, _, ContextError>(read_info.len)
                                    .parse_next(&mut input)
                                    .expect("failed to read export data");

                                println!(
                                    "Export {:#X} @ {:#X} start bytes {:#X?}",
                                    export.serial_offset,
                                    orig_input.len() - input_before.len(),
                                    &data[..data.len().min(4)]
                                );

                                if !read_info.ignore {
                                    export.data.push(data);
                                }

                                continue 'parser_loop;
                            }
                        }
                    }

                    for raw_package in &mut raw_packages {
                        for export in &mut raw_package.exports {
                            if export.serial_offset == read_info.export.serial_offset {
                                println!("Possible? {:#X?}", export);
                            }
                        }
                    }

                    println!("Expected: {:#X?}", read_info);
                    panic!(
                        "Unexpeced tag {tag:#08X} at {:#X}",
                        orig_input.len() - input.len()
                    );
                }
            }
        }
    }

    LinearFile {
        file_table: file_table,
        packages: raw_packages,
    }
}
