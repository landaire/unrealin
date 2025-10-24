use std::{
    io::{Cursor, IoSliceMut},
    path::PathBuf,
};

use clap::Parser;
use color_eyre::{
    Result,
    eyre::{self, Context, eyre},
};
use flate2::read::ZlibDecoder;
use memmap2::Mmap;
use winnow::{
    BStr, Parser as _,
    binary::{le_u32, u8},
    combinator::repeat,
    error::ContextError,
    token::take,
};

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Where to extract files to. By default this will be the basename of the input file.
    /// For example, `common.lin` will extract to `common/`
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// File to extract
    input: PathBuf,
}

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
pub fn decode_compact_index(input: &mut &[u8]) -> winnow::Result<i32> {
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
struct FileEntry<'i> {
    name: &'i BStr,
    offset: u32,
    len: u32,
    unk: u32,
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
struct PackageHeader<'i> {
    version: u32,
    flags: u32,
    name_count: u32,
    name_offset: u32,
    export_count: u32,
    export_offset: u32,
    import_count: u32,
    import_offset: u32,
    unknown_data: &'i [u8],
    guid_a: u32,
    guid_b: u32,
    guid_c: u32,
    guid_d: u32,
}

fn read_var_string<'i>(input: &mut &'i [u8]) -> winnow::Result<&'i BStr> {
    let name_len = decode_compact_index(input)?;
    let name = take((name_len as usize).saturating_sub(1)).parse_next(input)?;
    if name_len > 0 {
        let _null_term = u8(input)?;
    }

    Ok(&BStr::new(name))
}

#[derive(Debug)]
struct Name<'i> {
    name: &'i BStr,
    flags: u32,
}

fn read_name<'i>(input: &mut &'i [u8]) -> winnow::Result<Name<'i>> {
    Ok(Name {
        name: read_var_string(input)?,
        flags: le_u32(input)?,
    })
}

#[derive(Debug)]
struct Import {
    class_package: i32,
    class_name: i32,
    package_index: u32,
    object_name: i32,
}

fn read_import<'i>(input: &mut &'i [u8]) -> winnow::Result<Import> {
    let class_package = decode_compact_index(input)?;

    let class_name = decode_compact_index(input)?;

    let package_index = le_u32(input)?;

    let object_name = decode_compact_index(input)?;

    Ok(Import {
        class_package,
        class_name,
        package_index,
        object_name,
    })
}

#[derive(Debug)]
struct ObjectExport {
    class_index: i32,
    super_index: i32,
    package_index: u32,
    object_name: i32,
    object_flags: u32,
    serial_size: i32,
    serial_offset: i32,
}

fn read_export(input: &mut &[u8]) -> winnow::Result<ObjectExport> {
    let class_index = decode_compact_index(input)?;
    let super_index = decode_compact_index(input)?;

    let package_index = le_u32(input)?;

    let object_name = decode_compact_index(input)?;

    let object_flags = le_u32(input)?;

    let serial_size = decode_compact_index(input)?;

    let serial_offset = if serial_size != 0 {
        decode_compact_index(input)?
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
    })
}

#[derive(Debug)]
struct GenerationInfo {
    export_count: u32,
    name_count: u32,
}

fn read_generation_info(input: &mut &[u8]) -> winnow::Result<GenerationInfo> {
    let export_count = le_u32(input)?;
    let name_count = le_u32(input)?;

    Ok(GenerationInfo {
        export_count,
        name_count,
    })
}

enum HeaderKind<'i> {
    FileTable(FileTable<'i>),
    Package(Package<'i>),
}

#[derive(Debug)]
struct FileTable<'i> {
    files: Vec<FileEntry<'i>>,
}

fn read_file_table<'i>(input: &mut &'i [u8]) -> winnow::Result<FileTable<'i>> {
    // Reset input to skip past most of the header
    let _ = take(0x10 as usize).parse_next(input)?;

    let file_entry_count = decode_compact_index(input)?;

    let file_table: Vec<FileEntry<'_>> =
        repeat(file_entry_count as usize, read_file_entry).parse_next(input)?;

    println!("{:#X?}", file_table);

    Ok(FileTable { files: file_table })
}

fn read_package_header<'i>(input: &mut &'i [u8]) -> winnow::Result<PackageHeader<'i>> {
    let version = le_u32(input)?;
    let flags = le_u32(input)?;
    let name_count = le_u32(input)?;
    let name_offset = le_u32(input)?;
    let export_count = le_u32(input)?;
    let export_offset = le_u32(input)?;
    let import_count = le_u32(input)?;
    let import_offset = le_u32(input)?;

    let unk = le_u32(input)?;
    println!("Unknown value: {:#X}", unk);

    let unk_data_count = decode_compact_index(input)?;
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
        unknown_data,
        guid_a,
        guid_b,
        guid_c,
        guid_d,
    })
}

#[derive(Debug)]
struct Package<'i> {
    header: PackageHeader<'i>,
    names: Vec<Name<'i>>,
    imports: Vec<Import>,
    exports: Vec<ObjectExport>,
}

fn read_package<'i>(input: &mut &'i [u8]) -> winnow::Result<Package<'i>> {
    let tag = le_u32(input)?;

    assert_eq!(tag, 0x9e2a83c1, "package tag mismatch");

    let header = read_package_header(input).expect("failed to read package header");

    let names: Vec<_> = repeat(header.name_count as usize, read_name)
        .parse_next(input)
        .expect("failed to parse names");

    let imports: Vec<_> = repeat(header.import_count as usize, read_import)
        .parse_next(input)
        .expect("failed to parse import");

    let exports: Vec<_> = repeat(header.export_count as usize, read_export)
        .parse_next(input)
        .expect("failed to parse export");

    Ok(Package {
        header,
        names,
        imports,
        exports,
    })
}

fn main() -> Result<()> {
    let mut args = Args::parse();

    let mut input_file = std::fs::File::open(&args.input)
        .wrap_err_with(|| format!("failed to open {:?}", &args.input))?;

    let mut mmap = unsafe { memmap2::Mmap::map(&input_file)? };
    let mut input = &mmap[..];

    let mut output_dir = if let Some(output_dir) = args.output.take() {
        output_dir
    } else {
        let Some(parent) = args.input.parent() else {
            return Err(eyre!("Input path {:?} has no parent", args.input));
        };

        let Some(stem) = args.input.file_stem() else {
            return Err(eyre!("Input path {:?} has no file stem", args.input));
        };

        parent.join(stem)
    };

    std::fs::create_dir_all(&output_dir)
        .wrap_err_with(|| format!("failed to create output dir {:?}", &output_dir))?;

    let output_path = output_dir.join("complete.bin");
    let mut out_file = std::fs::File::create(&output_path)
        .wrap_err_with(|| format!("failed to create output file {output_path:?}"))?;

    let mut out_data = Vec::new();

    // Read the first data block to get the decompressed size
    let uncompressed_data_size = {
        let block = read_block(&mut input).map_err(|_| eyre!("failed to read block"))?;
        let mut reader = ZlibDecoder::new(block.compressed_data);
        let mut bytes = [0u8; 4];
        let mut cursor = Cursor::new(bytes.as_mut_slice());
        std::io::copy(&mut reader, &mut cursor)
            .wrap_err("failed to read decompressed data size")?;

        u32::from_le_bytes(bytes)
    };

    out_data.reserve(uncompressed_data_size as usize);

    let compressed_data_size = {
        let block = read_block(&mut input).map_err(|_| eyre!("failed to read block"))?;
        let mut reader = ZlibDecoder::new(block.compressed_data);
        let mut bytes = [0u8; 4];
        let mut cursor = Cursor::new(bytes.as_mut_slice());
        std::io::copy(&mut reader, &mut cursor)
            .wrap_err("failed to read decompressed data size")?;

        u32::from_le_bytes(bytes)
    };

    let unk1 = {
        let block = read_block(&mut input).map_err(|_| eyre!("failed to read block"))?;
        let mut reader = ZlibDecoder::new(block.compressed_data);
        let mut bytes = [0u8; 4];
        let mut cursor = Cursor::new(bytes.as_mut_slice());
        std::io::copy(&mut reader, &mut cursor)
            .wrap_err("failed to read decompressed data size")?;

        u32::from_le_bytes(bytes)
    };

    let unk2 = {
        let block = read_block(&mut input).map_err(|_| eyre!("failed to read block"))?;
        let mut reader = ZlibDecoder::new(block.compressed_data);
        let mut bytes = [0u8; 4];
        let mut cursor = Cursor::new(bytes.as_mut_slice());
        std::io::copy(&mut reader, &mut cursor)
            .wrap_err("failed to read decompressed data size")?;

        u32::from_le_bytes(bytes)
    };

    println!("Decompressed meta blocks:");
    println!("uncompressed_size={uncompressed_data_size:#X}");
    println!("compressed_size={compressed_data_size:#X}");
    println!("unk1={unk1:#X}");
    println!("unk2={unk2:#X}");

    let mut block_count = 0;
    while !input.is_empty() {
        let block = read_block(&mut input).map_err(|_| eyre!("failed to read block"))?;
        let mut reader = ZlibDecoder::new(block.compressed_data);

        std::io::copy(&mut reader, &mut out_data).wrap_err("failed to decompress block data")?;
        block_count += 1;
    }

    println!("Compressed block count: {block_count:#X} ({block_count})");

    let mut out_data_slice = out_data.as_slice();

    std::io::copy(&mut out_data_slice, &mut out_file)
        .wrap_err_with(|| format!("failed to copy data to output file {output_path:?}"))?;

    let mut input = &out_data[..];

    let unk = le_u32::<_, ContextError>(&mut input).expect("failed to parse tag");

    println!("{:#X}", unk);

    let name = read_var_string(&mut input).expect("failed to read lin name");
    println!("{}", name);

    let tag = le_u32::<_, ContextError>(&mut input).expect("failed to parse tag");
    println!("{:#X}", unk2);

    match tag {
        0x9FE3C5A3 => {
            let file_table = read_file_table(&mut input).expect("failed to read file table");
        }
        0x9e2a83c1 => {
            let unk = le_u32::<_, ContextError>(&mut input).unwrap();
            let name = read_var_string(&mut input).unwrap();
        }
        _ => {
            return Err(eyre!("Unexpected package tag: {:#X}", tag));
        }
    }

    let package = read_package(&mut input).expect("failed to read package");
    println!("{:#X?}", package);

    for import in &package.imports {
        println!("Import: {:?}", package.names[import.object_name as usize]);
    }

    Ok(())
}
