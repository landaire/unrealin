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
pub struct ExportIndex(pub(crate) usize);

impl ExportIndex {
    pub fn from_raw(idx: i32) -> Self {
        assert!(idx > 0, "Invalid export index");

        ExportIndex(normalize_index(idx))
    }

    pub fn raw(&self) -> usize {
        self.0
    }
}

pub(crate) type WeakLinker = Weak<RefCell<Linker>>;
pub(crate) type RcLinker = Rc<RefCell<Linker>>;

/// Captured raw body bytes per export, keyed by 0-based export index.
/// Filled by `preload`'s capture frame so the per-package serializer
/// can write export bodies verbatim: the bytes engine actually
/// consumed for that export's `Serialize`, in original order.
#[derive(Default, Debug)]
pub struct CapturedBytes {
    pub bodies: std::collections::HashMap<usize, Vec<u8>>,
}

pub struct Linker {
    pub objects: HashMap<ExportIndex, RcUnrealObject>,
    pub name: String,
    pub package: RawPackage,
    pub reader_offset: u64,
    /// Absolute byte offset of this package's PKG_TAG within the
    /// underlying decompressed `.lin` source. All package-relative
    /// offsets (name_offset, import_offset, export_offset, each
    /// export's serial_offset) are translated by adding this when
    /// seeking the source.
    pub source_start: u64,
    pub captured: CapturedBytes,
}

impl Linker {
    pub fn new(name: String, package: RawPackage) -> Linker {
        Linker {
            objects: Default::default(),
            name,
            package,
            reader_offset: 0,
            source_start: 0,
            captured: CapturedBytes::default(),
        }
    }

    pub fn version(&self) -> u16 {
        (self.package.header.version & 0xFFFF) as u16
    }

    pub fn licensee_version(&self) -> u16 {
        ((self.package.header.version & 0xFFFF_0000) >> 16) as u16
    }

    pub fn find_export_by_name(&self, name: &str) -> Option<(ExportIndex, &ObjectExport)> {
        // FName comparison is case-insensitive in UE2; the FName table
        // interns by lowercased key. Match that here so e.g.
        // `"ESam.SamAMesh"` from `object_load_order` resolves to the
        // `samAMesh` export when the on-disk capitalization differs.
        let index = self.package.exports.iter().position(|export| {
            export.object_name(self).eq_ignore_ascii_case(name)
        })?;

        Some((ExportIndex(index), &self.package.exports[index]))
    }

    /// Resolve an export by walking a path of segments, where each segment must
    /// match an export whose `package_index` points to the previous match. The
    /// first segment must be top-level (`package_index == 0`, i.e. outer is the
    /// package itself).
    ///
    /// `parts` are the segments AFTER the linker (package) name. For full_name
    /// `"Engine.Console"`, `parts = ["Console"]` and we resolve to the top-level
    /// `Console` Class. For `"Engine.Engine.Console"`, `parts = ["Engine",
    /// "Console"]` and we resolve to the `ClassProperty` nested inside the
    /// `Engine` class. Disambiguates names that appear at multiple scopes.
    pub fn find_export_by_path(&self, parts: &[&str]) -> Option<(ExportIndex, &ObjectExport)> {
        if parts.is_empty() {
            return None;
        }
        let mut current_outer: i32 = 0;
        let mut current_idx: Option<usize> = None;
        for part in parts {
            let mut found: Option<usize> = None;
            for (i, export) in self.package.exports.iter().enumerate() {
                if export.package_index != current_outer {
                    continue;
                }
                if !export.object_name(self).eq_ignore_ascii_case(part) {
                    continue;
                }
                found = Some(i);
                break;
            }
            let i = found?;
            current_idx = Some(i);
            current_outer = (i as i32) + 1;
        }
        let idx = current_idx?;
        Some((ExportIndex(idx), &self.package.exports[idx]))
    }

    /// Match `ULinker::VerifyImport`: select the export whose
    /// `ObjectName + ClassName + ClassPackage` triple matches. Returns None
    /// if no triple match. Engine then falls back to native class lookup
    /// (which we don't model), so the import resolves to "no object" and
    /// load_object_by_full_name returns None.
    pub fn find_export_by_name_and_class(
        &self,
        name: &str,
        class_name: &str,
        class_package: &str,
    ) -> Option<(ExportIndex, &ObjectExport)> {
        for (i, export) in self.package.exports.iter().enumerate() {
            if !export.object_name(self).eq_ignore_ascii_case(name) {
                continue;
            }
            if !export.class_name(self).eq_ignore_ascii_case(class_name) {
                continue;
            }
            if !export.class_package(self).eq_ignore_ascii_case(class_package) {
                continue;
            }
            return Some((ExportIndex(i), export));
        }
        None
    }

    pub fn find_import_by_index(&self, index: ImportIndex) -> Option<&Import> {
        self.package.imports.get(index.0)
    }

    pub fn find_export_by_index(&self, index: ExportIndex) -> Option<&ObjectExport> {
        self.package.exports.get(index.0)
    }

    pub fn set_position(&mut self, pos: u64) {
        self.reader_offset = pos;
    }

    pub fn position(&self) -> u64 {
        self.reader_offset
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

    pub fn full_name(&self, linker: &Linker, this_index: ImportIndex) -> String {
        let mut parts: Vec<&str> = Vec::new();
        let mut cursor: i32 = -(this_index.0 as i32) - 1;
        while cursor != 0 {
            let idx = ImportIndex::from_raw(cursor);
            let import = &linker.package.imports[idx.0];
            parts.push(import.object_name(linker));
            cursor = import.package_index;
        }
        parts.reverse();
        parts.join(".")
    }
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

    pub fn full_name(&self, linker: &Linker) -> String {
        // Walk the `package_index` chain (export index when positive, import
        // when negative, terminator when 0) and prepend each segment.
        // Matches the QEMU plugin's `gobj_loaded_order` formatting (which
        // walks `Outer` chains in `GObjLoaded.AddItem`).
        let mut parts: Vec<String> = vec![self.object_name(linker).to_owned()];
        let mut cursor = self.package_index;
        while cursor != 0 {
            if cursor > 0 {
                let exp = &linker.package.exports[(cursor - 1) as usize];
                parts.push(exp.object_name(linker).to_owned());
                cursor = exp.package_index;
            } else {
                let imp = &linker.package.imports[(-cursor - 1) as usize];
                parts.push(imp.object_name(linker).to_owned());
                cursor = imp.package_index;
            }
        }
        parts.push(linker.name.clone());
        parts.reverse();
        parts.join(".")
    }

    /// The package name where the export's class type is defined. Engine's
    /// `VerifyImport` matches imports against `(ObjectName, ClassName,
    /// ClassPackage)` and ClassPackage means "the package where the class
    /// itself lives". For a class_index that's an import, this is *not* the
    /// import's `class_package` field (that's where Core.Class is — always
    /// `Core`). It's the top-level package found by walking the import's
    /// `package_index` chain.
    pub fn class_package<'p>(&self, linker: &'p Linker) -> &'p str {
        let index = self.class_index;
        if index == 0 {
            return "Core";
        }
        let header = &linker.package;
        if index < 0 {
            let mut idx = normalize_index(index);
            loop {
                let import = &header.imports[idx];
                if import.package_index == 0 {
                    return header.names[import.object_name as usize].name.as_str();
                }
                if import.package_index >= 0 {
                    return header.names[import.class_package as usize].name.as_str();
                }
                idx = normalize_index(import.package_index);
            }
        } else {
            // For an export class, the class package is just the current
            // linker's package.
            linker.name.as_str()
        }
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
    assert_eq!(
        tag,
        PKG_TAG,
        "Invalid linker tag (source_consumed={:#X}, trace_ops_consumed={})",
        reader.source_consumed(),
        reader.trace_ops_consumed()
    );

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

    // Don't truncate or pad: the LIN file's decompressed data blocks
    // (chunks 4..N) sum to slightly more than `uncompressed_data_size`
    // due to zlib block alignment, but those trailing bytes ARE part
    // of the engine's reader's data buffer. Verified against
    // `splintercell.xbe.bndb`: the engine's compressed-file reader
    // (`initialize_linear_loader` @ `0x1A2D0`) reads chunks 0..3 as
    // metadata u32s and the rest as data. The texture_cache / VB / IB
    // regions are allocated SEPARATELY via three independent
    // `MmAllocateContiguousMemoryEx` calls (xbe `D3D_AllocContiguousMemory`
    // @ `0x1F29A0`); they're NOT in the same buffer as the file
    // data. So the engine has two truly independent readers; reads
    // past `uncompressed_data_size` for one `.lin` shouldn't happen
    // in a correct trace replay.
    Ok(out_data)
}

pub struct LinearFileDecoder<E, R> {
    sources: VecDeque<R>,
    metadata: ExportedData,
    file_table: Vec<FileEntry>,
    /// Package names for secondary `.lin` sources (source 1, 2, ...),
    /// captured from each source's LIN-format prefix at startup. The
    /// bootstrap routes `None.MyLevel` to `<this_name>.MyLevel` so the
    /// resolver loads the right level package regardless of which map
    /// the user is dumping (`menu`, `0_0_2_Training`, etc.).
    secondary_package_names: Vec<String>,
    runtime: UnrealRuntime,
    _endian: PhantomData<E>,
}

/// Compute the engine's expected `source_consumed` after popping each
/// trace op. Pre-trace: file_table (0x2C5AD) + Engine.u + Core.u headers
/// (op_idx == 0). Subsequent cascades fire at the op_idx recorded in
/// `file_load_op_index`; we add those linkers' header bytes once we
/// reach that index.
///
/// We don't know per-linker header byte counts from the trace alone,
/// so this stops computing past the first cascade event whose linker
/// sizes we don't have. For the run from initial load through the
/// Echelon cascade (op 100267) and the trace ops up to the next
/// cascade (op 446904), our binary's header consumption matches the
/// engine's exactly (verified at 0x17F197 post-cascade), so we
/// hard-code that boundary and the trace-read sums.
fn compute_expected_source_per_op(metadata: &ExportedData) -> Option<Vec<u64>> {
    if metadata.file_load_op_index.is_empty() {
        return None;
    }
    // pre_trace = file_table parse (0x2C5AD) + Engine.u + Core.u headers.
    // After Echelon's cascade (43 more linkers loaded together at op_idx
    // 100267), our log shows total = 0x17F197.
    const PRE_TRACE: u64 = 0x50CDA;
    const POST_FIRST_CASCADE: u64 = 0x17F197;

    // Find the op_idx of the second cascade event (anything past
    // file_load_op_index[0..2] = pre-trace). That's where our
    // hard-coded knowledge ends.
    let first_cascade_idx = *metadata.file_load_op_index.iter().find(|&&x| x > 0)?;
    let next_cascade_idx = *metadata
        .file_load_op_index
        .iter()
        .find(|&&x| x > first_cascade_idx)
        .unwrap_or(&usize::MAX);

    let mut out: Vec<u64> = Vec::with_capacity(metadata.raw_io_ops.len());
    let mut source: u64 = PRE_TRACE;
    let mut cascade_applied = false;
    for (i, op) in metadata.raw_io_ops.iter().enumerate() {
        if i >= next_cascade_idx {
            break;
        }
        // Apply Echelon cascade ONCE, right at the moment its op_idx
        // is reached (i.e. before processing op `first_cascade_idx`).
        if !cascade_applied && i >= first_cascade_idx {
            source = POST_FIRST_CASCADE;
            cascade_applied = true;
        }
        if let IoOp::Read { len, .. } = op {
            source += *len;
        }
        out.push(source);
    }
    Some(out)
}

impl<E, R> LinearFileDecoder<E, LinReader<R>>
where
    E: ByteOrder,
    R: Read,
{
    /// Build an unchecked decoder over the given `.lin` sources. No
    /// recorded I/O trace is required; the decoder relies on engine-
    /// correct deserialization to consume each source linearly. Source 0
    /// is treated as `common.lin` (file_table holder), sources 1+ have
    /// their LIN-format prefix consumed up front and contribute their
    /// package name to `secondary_package_names`.
    pub fn new_unchecked(mut sources: Vec<R>) -> Self {
        let mut secondary_package_names = Vec::with_capacity(sources.len().saturating_sub(1));
        for source in sources.iter_mut().skip(1) {
            let name = skip_secondary_lin_header::<E, _>(source)
                .expect("failed to skip secondary lin header");
            secondary_package_names.push(name);
        }
        let combined = LinReader::new_multi(sources);
        Self {
            sources: VecDeque::from(vec![combined]),
            runtime: UnrealRuntime {
                linkers: HashMap::new(),
                objects_full_loading: Default::default(),
                loaded_objects: Default::default(),
                package_file_size: HashMap::new(),
                // present_packages stays empty until `read_lin_header`
                // populates it from the file_table; we don't have a
                // recorded `file_load_order` to seed from.
                present_packages: Default::default(),
                pending_loads: Vec::new(),
                begin_load_count: 0,
                next_construction_index: 0,
                preload_stack: Vec::new(),
                file_table_entries: Vec::new(),
            },
            file_table: Vec::new(),
            secondary_package_names,
            metadata: ExportedData::default(),
            _endian: PhantomData,
        }
    }
}

impl<E, R> LinearFileDecoder<E, CheckedLinReader<R>>
where
    E: ByteOrder,
    R: Read,
{
    pub fn new_checked(mut sources: Vec<R>, mut metadata: ExportedData) -> Self {
        // Engine's `CreateFileReader` consumes each `.lin` file's
        // LIN-format prefix (`u32 load_address + packed_int name_len +
        // ANSI name`) at file-open time. The reader's `decompressed_size`
        // then refers to the post-prefix data only. Source 0 (common.lin)
        // is handled by `read_lin_header` via the regular read path
        // because it has a file_table to parse anyway; source 1+ skip
        // their prefix here. We capture the package name from each
        // secondary source's prefix so the bootstrap can route
        // `None.MyLevel` to the correct level package without
        // hard-coding `"menu"` (works for any map: training, abattoir,
        // etc.).
        let mut secondary_package_names = Vec::with_capacity(sources.len().saturating_sub(1));
        for source in sources.iter_mut().skip(1) {
            let name = skip_secondary_lin_header::<E, _>(source)
                .expect("failed to skip secondary lin header");
            secondary_package_names.push(name);
        }
        let expected_source_per_op = compute_expected_source_per_op(&metadata);
        let io_ops = Rc::new(RefCell::new(metadata.raw_io_ops.drain(..).collect()));
        let file_ptr_order = metadata.file_ptr_order.clone();
        // Single CheckedLinReader holds all sources and switches between
        // them based on each op's `file_ptr`. The trace's
        // `file_ptr_order` (already reversed by bin.rs to consumption
        // order) tells us which source each new file_ptr value maps to.
        let mut combined = CheckedLinReader::new(sources, file_ptr_order, io_ops);
        combined.expected_source_per_op = expected_source_per_op;
        Self {
            sources: VecDeque::from(vec![combined]),
            runtime: UnrealRuntime {
                linkers: HashMap::with_capacity(metadata.file_load_order.len()),
                objects_full_loading: Default::default(),
                loaded_objects: Default::default(),
                package_file_size: HashMap::new(),
                present_packages: metadata
                    .file_load_order
                    .iter()
                    .filter_map(|p| package_name_from_path(p))
                    .collect(),
                pending_loads: Vec::new(),
                begin_load_count: 0,
                next_construction_index: 0,
                preload_stack: Vec::new(),
                file_table_entries: Vec::new(),
            },
            file_table: Vec::new(),
            secondary_package_names,
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
    pub fn linkers(&self) -> &HashMap<String, Rc<RefCell<Linker>>> {
        &self.runtime.linkers
    }

    pub fn runtime_mut(&mut self) -> &mut crate::runtime::UnrealRuntime {
        &mut self.runtime
    }

    pub fn trace_ops_consumed(&self) -> u64 {
        self.sources
            .front()
            .map(|r| r.trace_ops_consumed())
            .unwrap_or(0)
    }

    /// Bytes consumed per `.lin` source so far. Index 0 is `common.lin`,
    /// index 1+ are the secondary `.lin`s passed to `new_unchecked`.
    /// Comparing each against the input file size shows whether the
    /// cascade left physical packages untouched at the tail of a source.
    pub fn source_consumed_per_source(&self) -> Vec<u64> {
        self.sources
            .front()
            .map(|r| r.source_consumed_per_source().to_vec())
            .unwrap_or_default()
    }

    pub fn trace_ops_remaining(&self) -> usize {
        self.sources
            .front()
            .map(|r| r.trace_ops_remaining())
            .unwrap_or(0)
    }

    /// Map from lowercased package name (the linker key style — `"engine"`,
    /// `"hud"`) to its original relative path with forward-slash separators
    /// (`"System/Engine.u"`, `"Textures/HUD.utx"`, `"Maps/menu.unr"`). Drawn
    /// from common.lin's `file_table`; the leading `..\` (relative to the
    /// game's `System` directory) is stripped so callers can join the
    /// result onto an output dir to reconstruct the original tree.
    pub fn package_filenames(&self) -> HashMap<String, String> {
        let mut map = HashMap::new();
        for entry in &self.file_table {
            let mut path = entry.name.as_str();
            // Strip leading `..\` (or repeated occurrences). Engine paths
            // are always relative to `<game>\System\`, so a single `..\`
            // walks up one level — but be defensive in case any entry has
            // a different prefix.
            while let Some(rest) = path.strip_prefix("..\\") {
                path = rest;
            }
            let normalized = path.replace('\\', "/");
            let leaf = normalized.rsplit('/').next().unwrap_or(&normalized);
            let stem = leaf.rsplit_once('.').map(|(s, _)| s).unwrap_or(leaf);
            if !stem.is_empty() {
                map.insert(stem.to_ascii_lowercase(), normalized);
            }
        }
        map
    }

    fn reader(&mut self) -> &mut R {
        self.sources.front_mut().expect("no file reader available?")
    }

    /// Bootstrap a level without a recorded I/O trace. Replays the
    /// engine's pre-MyLevel class warmup (153 hardcoded objects from
    /// xbe `game_main`'s `StaticLoadObject` calls), then loads the
    /// secondary `.lin`'s `MyLevel` to trigger the level cascade.
    ///
    /// The two tables in `engine_warmup` are derived from a recorded
    /// menu trace and apply to every map because `common.lin` is
    /// byte-identical across the SC NTSC build.
    pub fn decode_unchecked(&mut self) -> io::Result<()> {
        self.read_lin_header()?;
        self.runtime.present_packages.extend(
            crate::engine_warmup::COMMON_LIN_PACKAGES
                .iter()
                .map(|s| s.to_string()),
        );
        self.runtime
            .present_packages
            .extend(self.secondary_package_names.iter().cloned());

        // Stage 1: replay the pre-MyLevel warmup against source 0. Each
        // call is idempotent — duplicates that the cascade already
        // pulled in just no-op via `runtime.loaded_objects`.
        for object in crate::engine_warmup::ENGINE_CLASS_WARMUP {
            let reader = self.sources.front_mut().expect("no file reader available?");
            self.runtime.begin_load();
            self.runtime.load_object_by_full_name::<E, _>(
                object,
                crate::runtime::LoadKind::Load,
                reader,
            )?;
            self.runtime.end_load::<E, _>(reader)?;
        }

        // Stage 2: load `<secondary>.MyLevel` from source 1. The
        // ULevel cascade pulls in the post-MyLevel objects the menu
        // trace records (game info, HUD, controller, etc.) via actor
        // class refs.
        let reader = self.sources.front_mut().expect("no file reader available?");
        let secondary = self
            .secondary_package_names
            .first()
            .cloned()
            .expect("unchecked decode requires a secondary .lin source");
        reader.switch_to_source(1);
        let target = format!("{secondary}.MyLevel");
        self.runtime.begin_load();
        self.runtime.load_object_by_full_name::<E, _>(
            &target,
            crate::runtime::LoadKind::Load,
            reader,
        )?;
        self.runtime.end_load::<E, _>(reader)?;

        Ok(())
    }

    pub fn decode_linear_file(&mut self) -> io::Result<()> {
        self.read_lin_header()?;

        for object in &self.metadata.object_load_order {
            let reader = self.sources.front_mut().expect("no file reader available?");
            println!("Loading {object}");
            // `None.MyLevel` is the engine's bootstrap marker for the
            // level-package load. SC's `game_main` (xbe `0x1F370`)
            // translates this into `StaticLoadObject(MapName, ULevel,
            // ...)` where MapName is the per-region map subdir
            // (typically the same as the secondary `.lin`'s package
            // name: `"menu"` for menu.lin, `"0_0_2_Training"` for
            // training, etc.). The level package lives in a separate
            // `.lin` source, so we explicitly switch to source 1
            // before invoking `load_object_by_full_name`. Routing
            // through `<secondary_name>.MyLevel` triggers the
            // `ULevel::Serialize` stub via the existing module
            // resolver.
            let resolved = if object == "None.MyLevel" {
                if let Some(secondary) = self.secondary_package_names.first() {
                    reader.switch_to_source(1);
                    Some(format!("{secondary}.MyLevel"))
                } else {
                    None
                }
            } else {
                None
            };
            let target: &str = resolved.as_deref().unwrap_or(object.as_str());
            self.runtime.begin_load();
            self.runtime.load_object_by_full_name::<E, _>(
                target,
                crate::runtime::LoadKind::Load,
                reader,
            )?;
            self.runtime.end_load::<E, _>(reader)?;
        }

        Ok(())
    }

    pub fn read_lin_header(&mut self) -> io::Result<()> {
        let has_file_table = !self.file_table.is_empty();

        let reader = self.reader();

        reader.set_reading_linker_header(true);

        let _unk = reader.read_u32::<E>()?;
        let _name = reader.read_string()?;

        if has_file_table {
            reader.set_reading_linker_header(false);
            return Ok(());
        }

        let tag = reader.read_u32::<E>()?;
        assert_eq!(tag, LIN_FILE_TABLE_TAG, "LIN file table tag mismatch");

        let file_table = read_file_table::<E, _>(reader).expect("failed to read file table");
        reader.set_reading_linker_header(false);

        for entry in &file_table {
            if let Some(pkg) = package_name_from_path(&entry.name) {
                self.runtime
                    .package_file_size
                    .insert(pkg.clone(), entry.len as u64);
                // Mark the package as physically present in this `.lin` so
                // `verify_imports` will cascade-load it. Without this gate
                // any package that's in the file_table but not in our
                // hardcoded warmup list is treated as an engine intrinsic
                // and skipped — leaving its header bytes in the source
                // unread, which then misaligns subsequent reads.
                self.runtime.present_packages.insert(pkg);
            }
        }

        // Mirror the full file_table to the runtime so non-package lookups
        // (lipsynch `.bin`, language-localized assets) can resolve a body-stored
        // filename to its on-disk size. Stored lowercased so suffix lookups
        // are case-insensitive.
        self.runtime.file_table_entries = file_table
            .iter()
            .map(|entry| (entry.name.to_lowercase(), entry.len as u64))
            .collect();

        self.file_table = file_table;
        Ok(())
    }

}

/// Walk a decompressed secondary `.lin` (= `<map>.lin`) and collect
/// the top-level package names referenced by every package physically
/// in the source. Used to seed `runtime.present_packages` for the
/// unchecked decode path: the engine's `verify_imports` cascade
/// fires `GetPackageLinker(name)` for each top-level import, and we
/// need to know which of those names actually resolve to a package
/// in our `.lin` (vs an engine intrinsic that has no on-disk body).
///
/// The secondary `.lin` has no manifest beyond its single-name
/// LIN-format prefix, so we discover packages by:
///
///   1. Skipping the LIN-format prefix (`u32 load_address +
///      packed_int name_len + ANSI name`).
///   2. Sliding a 4-byte window across the rest of the data looking
///      for `PKG_TAG` (`0x9E2A83C1` LE).
///   3. For each candidate offset, attempting to parse a package
///      header. If the version field is `0x110064` (SC's
///      `Ver=0x64`, `LicenseeVer=0x11`) and `name_count`,
///      `name_offset`, `import_count`, `import_offset` all stay
///      within the source bounds, treat it as a real package and
///      parse its names + imports.
///   4. For each import with `package_index == 0` (top-level), look
///      up `object_name` in the package's names table. That name is
///      the imported package's name.
///   5. Return the union of all such names.
///
/// Some returned names are already in `COMMON_LIN_PACKAGES` (the
/// engine's `verify_imports` is called on every linker, common.lin
/// and map.lin alike). Adding them again is a no-op since
/// `present_packages` is a `HashSet`.
pub fn discover_secondary_package_names(data: &[u8]) -> Vec<String> {
    use std::collections::HashSet;
    use std::io::Cursor;

    let mut cursor = Cursor::new(data);
    if skip_secondary_lin_header::<byteorder::LittleEndian, _>(&mut cursor).is_err() {
        return Vec::new();
    }
    let prefix_end = cursor.position() as usize;

    let mut pkg_offsets: Vec<usize> = Vec::new();
    if data.len() >= prefix_end + 4 {
        for i in prefix_end..data.len() - 4 {
            if data[i] == 0xc1
                && data[i + 1] == 0x83
                && data[i + 2] == 0x2a
                && data[i + 3] == 0x9e
            {
                pkg_offsets.push(i);
            }
        }
    }

    let mut names: HashSet<String> = HashSet::new();
    for &off in &pkg_offsets {
        if let Some(pkg) = try_parse_package_at::<byteorder::LittleEndian>(data, off) {
            for imp in &pkg.imports {
                if imp.package_index == 0 {
                    if let Some(n) = pkg.names.get(imp.object_name as usize) {
                        if !n.name.is_empty() {
                            names.insert(n.name.clone());
                        }
                    }
                }
            }
        }
    }
    names.into_iter().collect()
}


/// Try to parse a UE2 package header starting at `data[offset]`.
/// Returns `None` if the bytes don't look like a valid SC package
/// header (wrong version, out-of-bounds offsets, etc.). Used by
/// `discover_secondary_package_names` to filter false-positive
/// PKG_TAG byte matches from real package starts.
fn try_parse_package_at<E: ByteOrder>(data: &[u8], offset: usize) -> Option<RawPackage> {
    use crate::reader::LinReader;
    use std::io::Cursor;

    let slice = data.get(offset..)?;
    let mut reader = LinReader::new(Cursor::new(slice));
    reader.set_source_start(0);

    let header = match read_package_header::<E, _>(&mut reader) {
        Ok(h) => h,
        Err(_) => return None,
    };
    // SC: Ver=0x64, LicenseeVer=0x11. The combined u32 LE is 0x00110064.
    if header.version != 0x00110064 {
        return None;
    }
    let max = (data.len() - offset) as u64;
    if header.name_offset as u64 > max
        || header.import_offset as u64 > max
        || header.export_offset as u64 > max
    {
        return None;
    }

    if reader
        .seek(SeekFrom::Start(header.name_offset as u64))
        .is_err()
    {
        return None;
    }
    let mut names = Vec::with_capacity(header.name_count as usize);
    for _ in 0..header.name_count as usize {
        match read_name::<E, _>(&mut reader) {
            Ok(n) => names.push(n),
            Err(_) => return None,
        }
    }

    if reader
        .seek(SeekFrom::Start(header.import_offset as u64))
        .is_err()
    {
        return None;
    }
    let mut imports = Vec::with_capacity(header.import_count as usize);
    for _ in 0..header.import_count as usize {
        match read_import::<E, _>(&mut reader) {
            Ok(i) => imports.push(i),
            Err(_) => return None,
        }
    }

    Some(RawPackage {
        header,
        names,
        imports,
        exports: Vec::new(),
    })
}

/// Read past a secondary `.lin` source's LIN-format prefix:
/// `u32 load_address + packed_int name_len + ANSI name`. Mirrors
/// what the engine's `CreateFileReader` does at file-open time —
/// those bytes never appear in the file-data stream, only in the
/// reader's metadata. Returns the decoded package name (e.g.
/// `"menu"` for `menu.lin`, `"0_0_2_Training"` for the training
/// map's `.lin`) so the bootstrap can route `None.MyLevel` to the
/// right secondary package.
fn skip_secondary_lin_header<E, R>(source: &mut R) -> io::Result<String>
where
    E: ByteOrder,
    R: Read,
{
    let mut tmp = [0u8; 4];
    source.read_exact(&mut tmp)?; // load_address
    let mut byte = [0u8; 1];
    source.read_exact(&mut byte)?;
    let b0 = byte[0];
    let mut value: u32 = 0;
    if (b0 & 0x40) != 0 {
        source.read_exact(&mut byte)?;
        let b1 = byte[0];
        if (b1 & 0x80) != 0 {
            source.read_exact(&mut byte)?;
            let b2 = byte[0];
            if (b2 & 0x80) != 0 {
                source.read_exact(&mut byte)?;
                let b3 = byte[0];
                if (b3 & 0x80) != 0 {
                    source.read_exact(&mut byte)?;
                    value = byte[0] as u32;
                }
                value = (value << 7) + ((b3 & 0x7f) as u32);
            }
            value = (value << 7) + ((b2 & 0x7f) as u32);
        }
        value = (value << 7) + ((b1 & 0x7f) as u32);
    }
    value = (value << 6) + ((b0 & 0x3f) as u32);
    let mut name_buf = vec![0u8; value as usize];
    source.read_exact(&mut name_buf)?;
    // Trim trailing NUL terminator from the ANSI name.
    if name_buf.last() == Some(&0) {
        name_buf.pop();
    }
    Ok(String::from_utf8_lossy(&name_buf).into_owned())
}

fn package_name_from_path(path: &str) -> Option<String> {
    let leaf = path.rsplit('\\').next().unwrap_or(path);
    let stem = leaf.rsplit_once('.').map(|(s, _)| s).unwrap_or(leaf);
    if stem.is_empty() { None } else { Some(stem.to_owned()) }
}
