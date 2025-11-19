use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
    io::{BufRead, Cursor, ErrorKind, Read, Seek, SeekFrom},
    marker::PhantomData,
    rc::{Rc, Weak},
};

use crate::{
    object::{
        DeserializeUnrealObject, ObjectFlags, RcUnrealObject, UObjectKind, UnrealObject,
        builtins::*,
    },
    reader::{CheckedLinReader, LinRead, LinReader, UnrealReadExt},
    runtime::UnrealRuntime,
};
use byteorder::{ByteOrder, ReadBytesExt};
use flate2::read::ZlibDecoder;
use serde::Deserialize;
use std::io;

use crate::common::normalize_index;
use crate::{
    LIN_FILE_TABLE_TAG, PKG_TAG,
    common::{ExportRead, ExportedData, IoOp},
};

#[derive(Copy, Clone, Eq, PartialEq, PartialOrd, Ord, Hash)]
pub(crate) struct ImportIndex(usize);
impl ImportIndex {
    pub fn from_raw(idx: i32) -> Self {
        assert!(idx < 0, "Invalid import index");

        ImportIndex(normalize_index(idx))
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct ExportIndex(usize);

impl ExportIndex {
    pub fn from_raw(idx: i32) -> Self {
        assert!(idx > 0, "Invalid export index");

        ExportIndex(normalize_index(idx))
    }
}

pub(crate) type WeakLinker = Weak<RefCell<Linker>>;
pub(crate) type RcLinker = Rc<RefCell<Linker>>;

pub(crate) struct Linker {
    pub objects: HashMap<ExportIndex, RcUnrealObject>,
    pub name: String,
    pub package: RawPackage,
}

impl Linker {
    pub fn new(name: String, package: RawPackage) -> Linker {
        Linker {
            objects: Default::default(),
            name,
            package,
        }
    }

    pub fn version(&self) -> u16 {
        (self.package.header.version & 0xFFFF) as u16
    }

    pub fn licensee_version(&self) -> u16 {
        ((self.package.header.version & 0xFFFF_0000) >> 16) as u16
    }

    pub fn find_export_by_name(&self, name: &str) -> Option<(ExportIndex, &ObjectExport)> {
        let index = self
            .package
            .exports
            .iter()
            .position(|export| export.object_name(self) == name)?;

        Some((ExportIndex(index), &self.package.exports[index]))
    }

    pub fn find_import_by_index(&self, index: ImportIndex) -> Option<&Import> {
        self.package.imports.get(index.0)
    }

    pub fn find_export_by_index(&self, index: ExportIndex) -> Option<&ObjectExport> {
        self.package.exports.get(index.0)
    }
}

struct Block {
    uncompressed_len: u32,
    compressed_len: u32,
    compressed_data: Vec<u8>,
}

fn read_block<E, R>(reader: &mut R) -> io::Result<Block>
where
    R: Read,
    E: ByteOrder,
{
    let uncompressed_len = reader.read_u32::<E>()?;
    let compressed_len = reader.read_u32::<E>()?;
    let mut compressed_data = vec![0u8; compressed_len as usize];
    reader.read_exact(&mut compressed_data)?;

    Ok(Block {
        uncompressed_len,
        compressed_len,
        compressed_data,
    })
}

#[derive(Debug)]
pub(crate) struct FileEntry {
    pub name: String,
    pub offset: u32,
    pub len: u32,
    pub unk: u32,
}

fn read_file_entry<E, R>(reader: &mut R) -> io::Result<FileEntry>
where
    R: LinRead,
    E: ByteOrder,
{
    let name = reader.read_string()?;
    let offset = reader.read_u32::<E>()?;
    let len = reader.read_u32::<E>()?;
    let unk = reader.read_u32::<E>()?;

    let entry = FileEntry {
        name,
        offset,
        len,
        unk,
    };

    Ok(entry)
}

#[derive(Debug)]
pub struct PackageHeader {
    pub version: u32,
    pub flags: u32,
    pub name_count: u32,
    pub name_offset: u32,
    pub export_count: u32,
    pub export_offset: u32,
    pub import_count: u32,
    pub import_offset: u32,
    pub unk: u32,
    pub unknown_data: Vec<u8>,
    pub guid_a: u32,
    pub guid_b: u32,
    pub guid_c: u32,
    pub guid_d: u32,
    pub generations: Vec<GenerationInfo>,
}

#[derive(Debug)]
pub struct Name {
    pub name: String,
    pub flags: u32,
}

fn read_name<E, R>(reader: &mut R) -> io::Result<Name>
where
    R: LinRead,
    E: ByteOrder,
{
    Ok(Name {
        name: reader.read_string()?,
        flags: reader.read_u32::<E>()?,
    })
}

#[derive(Debug)]
pub(crate) struct Import {
    pub class_package: i32,
    pub class_name: i32,
    pub package_index: i32,
    pub object_name: i32,
}

impl Import {
    pub fn class_name<'p>(&self, package: &'p Linker) -> &'p str {
        package.package.names[self.class_name as usize]
            .name
            .as_str()
    }

    pub fn object_name<'p>(&self, package: &'p Linker) -> &'p str {
        package.package.names[self.object_name as usize]
            .name
            .as_str()
    }

    pub fn full_name<'p>(&self, linker: &'p Linker) -> String {
        let package_name = &linker.package.names[self.class_package as usize];
        format!("{}.{}", &package_name.name, self.object_name(linker))
    }

    // pub fn full_name(&self, package: &RawPackage<'_>) -> String {
    //     format!(
    //         "{} {}.{}",
    //         package.names[self.class_name as usize].name,
    //         package.names[self.class_package as usize].name,
    //         package.names[self.object_name as usize].name
    //     )
    // }

    // pub fn resolve_export<'i>(&self, container: &'i RawPackage<'_>) -> &'i ObjectExport<'i> {
    //     let normalized_index = normalize_index(self.package_index);
    //     &container.exports[normalized_index]
    // }
}

fn read_import<E, R>(reader: &mut R) -> io::Result<Import>
where
    R: LinRead,
    E: ByteOrder,
{
    let class_package = reader.read_packed_int()?;

    let class_name = reader.read_packed_int()?;

    let package_index = reader.read_i32::<E>()?;

    let object_name = reader.read_packed_int()?;

    Ok(Import {
        class_package,
        class_name,
        package_index,
        object_name,
    })
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Hash)]
pub struct ObjectExport {
    pub class_index: i32,
    pub super_index: i32,
    pub package_index: i32,
    pub object_name: i32,
    pub object_flags: u32,
    pub serial_size: i32,
    pub serial_offset: i32,
}

impl ObjectExport {
    fn partially_eq(&self, other: &Self) -> bool {
        self.class_index == other.class_index
            && self.super_index == other.super_index
            && self.package_index == other.package_index
            && self.object_flags == other.object_flags
            && self.serial_size == other.serial_size
            && self.serial_offset == other.serial_offset
    }

    pub fn serial_offset(&self) -> u64 {
        self.serial_offset as u64
    }

    pub fn serial_size(&self) -> usize {
        self.serial_size as usize
    }
}

impl ObjectExport {
    pub fn object_name<'p>(&self, linker: &'p Linker) -> &'p str {
        linker.package.names[self.object_name as usize]
            .name
            .as_str()
    }

    pub fn class_name<'p>(&self, linker: &'p Linker) -> &'p str {
        let index = self.class_index;

        if index == 0 {
            return "Class";
        }

        let header = &linker.package;
        if index < 0 {
            header.names[header.imports[normalize_index(index)].object_name as usize]
                .name
                .as_str()
        } else {
            header.names[header.exports[normalize_index(index)].object_name as usize]
                .name
                .as_str()
        }
    }

    pub fn full_name<'p>(&self, linker: &'p Linker) -> String {
        format!("{}.{}", &linker.name, self.object_name(linker))
    }
}

fn read_export<E, R>(reader: &mut R) -> io::Result<ObjectExport>
where
    R: LinRead,
    E: ByteOrder,
{
    let class_index = reader.read_packed_int()?;
    let super_index = reader.read_packed_int()?;

    let package_index = reader.read_i32::<E>()?;

    let object_name = reader.read_packed_int()?;

    let object_flags = reader.read_u32::<E>()?;

    let serial_size = reader.read_packed_int()?;

    assert!(serial_size >= 0, "serial_size cannot be negative");

    let serial_offset = if serial_size > 0 {
        reader.read_packed_int()?
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
pub(crate) struct GenerationInfo {
    pub export_count: u32,
    pub name_count: u32,
}

fn read_generation_info<E, R>(reader: &mut R) -> io::Result<GenerationInfo>
where
    R: Read,
    E: ByteOrder,
{
    let export_count = reader.read_u32::<E>()?;
    let name_count = reader.read_u32::<E>()?;

    Ok(GenerationInfo {
        export_count,
        name_count,
    })
}

fn read_file_table<E, R>(reader: &mut R) -> io::Result<Vec<FileEntry>>
where
    R: LinRead,
    E: ByteOrder,
{
    // Reset input to skip past most of the header
    let mut garbage = [0u8; 0x10];
    reader.read_exact(&mut garbage)?;

    let file_entry_count = reader.read_packed_int()? as usize;
    let mut file_table: Vec<FileEntry> = Vec::with_capacity(file_entry_count);
    for _ in 0..file_entry_count {
        file_table.push(read_file_entry::<E, _>(reader)?);
    }

    Ok(file_table)
}

fn read_package_header<E, R>(reader: &mut R) -> io::Result<PackageHeader>
where
    R: LinRead,
    E: ByteOrder,
{
    let tag = reader.read_u32::<E>()?;
    assert_eq!(tag, PKG_TAG, "Invalid linker tag");

    let version = reader.read_u32::<E>()?;
    println!("Version: {:#X}", version);
    let flags = reader.read_u32::<E>()?;
    let name_count = reader.read_u32::<E>()?;
    println!("name_count: {:#X}", name_count);
    let name_offset = reader.read_u32::<E>()?;
    let export_count = reader.read_u32::<E>()?;
    let export_offset = reader.read_u32::<E>()?;
    let import_count = reader.read_u32::<E>()?;
    let import_offset = reader.read_u32::<E>()?;

    let unk = reader.read_u32::<E>()?;
    println!("Unknown value: {:#X}", unk);

    let unknown_data = reader.read_array()?;

    let guid_a = reader.read_u32::<E>()?;
    let guid_b = reader.read_u32::<E>()?;
    let guid_c = reader.read_u32::<E>()?;
    let guid_d = reader.read_u32::<E>()?;

    let generation_count = reader.read_u32::<E>()? as usize;
    let mut generations = Vec::with_capacity(generation_count);
    for _ in 0..generation_count {
        generations.push(read_generation_info::<E, _>(reader)?);
    }

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
pub struct RawPackage {
    pub header: PackageHeader,
    pub names: Vec<Name>,
    pub imports: Vec<Import>,
    pub exports: Vec<ObjectExport>,
}

pub fn read_package<E, R>(reader: &mut R) -> io::Result<RawPackage>
where
    R: LinRead,
    E: ByteOrder,
{
    let header = read_package_header::<E, _>(reader)?;

    reader.seek(SeekFrom::Start(header.name_offset as u64))?;

    let mut names = Vec::with_capacity(header.name_count as usize);
    for _ in 0..header.name_count as usize {
        names.push(read_name::<E, _>(reader)?);
    }

    reader.seek(SeekFrom::Start(header.import_offset as u64))?;
    let mut imports = Vec::with_capacity(header.import_count as usize);
    for _ in 0..header.import_count as usize {
        imports.push(read_import::<E, _>(reader)?);
    }

    reader.seek(SeekFrom::Start(header.export_offset as u64))?;
    let mut exports = Vec::with_capacity(header.export_count as usize);
    for _ in 0..header.export_count as usize {
        exports.push(read_export::<E, _>(reader)?);
    }

    Ok(RawPackage {
        header,
        names,
        imports,
        exports,
    })
}

pub fn decompress_linear_file<E, R>(reader: &mut R) -> io::Result<Vec<u8>>
where
    R: Read,
    E: ByteOrder,
{
    let mut out_data = Vec::new();

    // Read the first data block to get the decompressed size
    let uncompressed_data_size = {
        let block = read_block::<E, _>(reader).expect("failed to read block");
        let mut reader = ZlibDecoder::new(block.compressed_data.as_slice());
        let mut bytes = [0u8; 4];
        let mut cursor = Cursor::new(bytes.as_mut_slice());
        std::io::copy(&mut reader, &mut cursor).expect("failed to read zlib data ");

        u32::from_le_bytes(bytes)
    };

    out_data.reserve(uncompressed_data_size as usize);

    let compressed_data_size = {
        let block = read_block::<E, _>(reader).expect("failed to read block");
        let mut reader = ZlibDecoder::new(block.compressed_data.as_slice());
        let mut bytes = [0u8; 4];
        let mut cursor = Cursor::new(bytes.as_mut_slice());
        std::io::copy(&mut reader, &mut cursor).expect("failed to read zlib data");

        u32::from_le_bytes(bytes)
    };

    let unk1 = {
        let block = read_block::<E, _>(reader).expect("failed to read block");
        let mut reader = ZlibDecoder::new(block.compressed_data.as_slice());
        let mut bytes = [0u8; 4];
        let mut cursor = Cursor::new(bytes.as_mut_slice());
        std::io::copy(&mut reader, &mut cursor).expect("failed to read zlib data");

        u32::from_le_bytes(bytes)
    };

    let unk2 = {
        let block = read_block::<E, _>(reader).expect("failed to read block");
        let mut reader = ZlibDecoder::new(block.compressed_data.as_slice());
        let mut bytes = [0u8; 4];
        let mut cursor = Cursor::new(bytes.as_mut_slice());
        std::io::copy(&mut reader, &mut cursor).expect("failed to read zlib data");

        u32::from_le_bytes(bytes)
    };

    println!("uncompressed_data_size: {uncompressed_data_size:#X}");
    println!("compressed_data_size: {compressed_data_size:#X}");
    println!("unk1: {unk1:#X}");
    println!("unk2: {unk2:#X}");

    // Read until EOF
    loop {
        let block = match read_block::<E, _>(reader) {
            Ok(block) => block,
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => {
                break;
            }
            Err(e) => {
                // Unexpected error
                return Err(e);
            }
        };
        let mut reader = ZlibDecoder::new(block.compressed_data.as_slice());

        std::io::copy(&mut reader, &mut out_data).expect("failed to read zlib data");
    }

    Ok(out_data)
}

pub struct LinearFileDecoder<E, R> {
    sources: VecDeque<R>,
    metadata: ExportedData,
    file_table: Vec<FileEntry>,
    runtime: UnrealRuntime,
    _endian: PhantomData<E>,
}

impl<E, R> LinearFileDecoder<E, LinReader<R>>
where
    E: ByteOrder,
    R: Read,
{
    pub fn new(sources: Vec<R>, metadata: ExportedData) -> Self {
        Self {
            sources: VecDeque::from_iter(sources.into_iter().map(LinReader::new)),
            runtime: UnrealRuntime {
                linkers: HashMap::with_capacity(metadata.file_load_order.len()),
            },
            file_table: Vec::new(),
            metadata,
            _endian: PhantomData,
        }
    }
}

impl<E, R> LinearFileDecoder<E, CheckedLinReader<R>>
where
    E: ByteOrder,
    R: Read,
{
    pub fn new_checked(sources: Vec<R>, mut metadata: ExportedData) -> Self {
        let io_ops = Rc::new(RefCell::new(metadata.raw_io_ops.drain(..).collect()));
        Self {
            sources: VecDeque::from_iter(
                sources
                    .into_iter()
                    .map(|reader| CheckedLinReader::new(reader, Rc::clone(&io_ops))),
            ),
            runtime: UnrealRuntime {
                linkers: HashMap::with_capacity(metadata.file_load_order.len()),
            },
            file_table: Vec::new(),
            metadata,
            _endian: PhantomData,
        }
    }
}

impl<E, R> LinearFileDecoder<E, R>
where
    E: ByteOrder,
    R: LinRead,
{
    fn reader(&mut self) -> &mut R {
        self.sources.front_mut().expect("no file reader available?")
    }

    pub fn decode_linear_file(&mut self) -> io::Result<()> {
        self.read_lin_header()?;

        for object in &self.metadata.object_load_order {
            let reader = self.sources.front_mut().expect("no file reader available?");
            println!("Loading {object}");
            self.runtime.load_object_by_full_name::<E, _>(
                object,
                crate::runtime::LoadKind::Load,
                reader,
            )?;
            panic!("first object loaded!");
        }

        Ok(())
    }

    pub fn read_lin_header(&mut self) -> io::Result<()> {
        let has_file_table = !self.file_table.is_empty();

        let mut reader = self.reader();

        reader.set_reading_linker_header(true);

        let unk = reader.read_u32::<E>()?;
        let name = reader.read_string()?;
        println!("{}", name);

        // There's only one file table, so we shouldn't read this.
        if has_file_table {
            reader.set_reading_linker_header(false);
            return Ok(());
        }

        let tag = reader.read_u32::<E>()?;
        assert_eq!(tag, LIN_FILE_TABLE_TAG, "LIN file table tag mismatch");

        let file_table = Some(read_file_table::<E, _>(reader).expect("failed to read file table"));
        println!(
            "File table length: {:#X}",
            file_table.as_ref().map(|t| t.len()).unwrap_or_default()
        );
        println!("{file_table:#X?}");

        reader.set_reading_linker_header(false);

        Ok(())
    }
}
