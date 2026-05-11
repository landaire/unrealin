use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::io::ErrorKind;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::marker::PhantomData;
use std::rc::Rc;
use std::rc::Weak;

use crate::object::RcUnrealObject;
use crate::reader::CheckedLinReader;
use crate::reader::LinRead;
use crate::reader::LinReader;
use crate::reader::UnrealReadExt;
use crate::runtime::UnrealRuntime;
use byteorder::ByteOrder;
use byteorder::ReadBytesExt;
use flate2::read::ZlibDecoder;
use serde::Deserialize;
use std::io;

use crate::LIN_FILE_TABLE_TAG;
use crate::PKG_TAG;
use crate::common::ExportedData;
use crate::common::IoOp;
use crate::common::normalize_index;

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Game {
    /// Retail/demo Splinter Cell 1 (NTSC/PAL Xbox). Native serialize
    /// code has version-gated fields keyed on a per-instance disk
    /// version stamp (e.g. UESoftBody at `Engine_demo.dll 0x103b59d0`
    /// gates on V > 2..0xa).
    SplinterCell,
    /// Splinter Cell 1 Sep-13-2002 prototype. Native serialize code
    /// is structurally similar to retail's but without the version
    /// gates: every field is read unconditionally and the proto
    /// stamps every save at V=9 (e.g. UESoftBody at
    /// `splintercell_proto.xbe sub_61a90`).
    SplinterCellPrototype,
    /// Splinter Cell: Pandora Tomorrow (Xbox). The `.lin` zlib
    /// wrapper carries a 5th metadata block (vs SC1's 4); see
    /// `decompress_linear_file_with_info` for the detection.
    PandoraTomorrow,
}

impl Default for Game {
    fn default() -> Self {
        Self::SplinterCell
    }
}

#[derive(Copy, Clone, Eq, PartialEq, PartialOrd, Ord, Hash)]
pub struct ImportIndex(usize);
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
    /// Index of the `.lin` source this package's bytes live in
    /// (`0` = `common.lin`, `1` = `<map>.lin`). Captured at
    /// `load_linker` time from the active `current_source_idx`. Used
    /// by `push_linker` in unchecked mode to switch the active source
    /// before reading this linker's export bodies -- necessary when a
    /// Stage-2 cascade running on src1 triggers a preload for an
    /// export whose body is in src0 (e.g. `ESam.samBMesh` referenced
    /// from `5_1_1_PresidentialPalace.MyLevel`'s actors).
    pub source_idx: usize,
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
            source_idx: 0,
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
        let index = self
            .package
            .exports
            .iter()
            .position(|export| export.object_name(self).eq_ignore_ascii_case(name))?;

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
            if !export
                .class_package(self)
                .eq_ignore_ascii_case(class_package)
            {
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
pub struct Import {
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

/// The five fields the export table emits per export row. Layout-
/// independent: `serial_size` and `serial_offset` are intentionally
/// excluded so the same struct can be used for both the on-disk
/// neutralised form and the size-accounting pass without re-listing
/// the field names.
///
/// Has no inherent methods so that `Deref<Target = ExportTableFields>`
/// on `ObjectExport` can't accidentally shadow callers' method lookups.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Hash)]
pub struct ExportTableFields {
    pub class_index: i32,
    pub super_index: i32,
    pub package_index: i32,
    pub object_name: i32,
    pub object_flags: u32,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Hash)]
pub struct ObjectExport {
    #[serde(flatten)]
    pub fields: ExportTableFields,
    pub serial_size: i32,
    pub serial_offset: i32,
}

impl std::ops::Deref for ObjectExport {
    type Target = ExportTableFields;
    fn deref(&self) -> &ExportTableFields {
        &self.fields
    }
}

impl std::ops::DerefMut for ObjectExport {
    fn deref_mut(&mut self) -> &mut ExportTableFields {
        &mut self.fields
    }
}

impl ObjectExport {
    /// SC's `SavePackage` neutral form for a never-loaded export:
    /// class/super/package/flags zeroed, `object_name = "None"`
    /// (caller passes the linker's name-table index for `"None"`).
    /// Used by the serializer to emit a parseable but inert table
    /// row for exports whose body we don't write.
    pub fn neutral(object_name: i32) -> Self {
        Self {
            fields: ExportTableFields {
                class_index: 0,
                super_index: 0,
                package_index: 0,
                object_name,
                object_flags: 0,
            },
            serial_size: 0,
            serial_offset: 0,
        }
    }

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
    /// import's `class_package` field (that's where Core.Class is -- always
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

    if serial_size < 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("serial_size cannot be negative ({serial_size})"),
        ));
    }

    let serial_offset = if serial_size > 0 {
        reader.read_packed_int()?
    } else {
        0
    };
    Ok(ObjectExport {
        fields: ExportTableFields {
            class_index,
            super_index,
            package_index,
            object_name,
            object_flags,
        },
        serial_size,
        serial_offset,
    })
}

#[derive(Debug)]
pub struct GenerationInfo {
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

fn read_pt_open_path_list<E, R>(reader: &mut R) -> io::Result<Vec<String>>
where
    R: Read,
    E: ByteOrder,
{
    let count = reader.read_u32::<E>()?;
    read_pt_open_path_list_entries::<E, _>(reader, count)
}

fn read_pt_open_path_list_entries<E, R>(reader: &mut R, count: u32) -> io::Result<Vec<String>>
where
    R: Read,
    E: ByteOrder,
{
    let mut paths = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let char_count = reader.read_u32::<E>()? as usize;
        let unit_count = char_count.checked_add(1).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "PT LIN path length overflow")
        })?;
        let byte_count = unit_count.checked_mul(2).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "PT LIN path length overflow")
        })?;
        let mut raw = vec![0u8; byte_count];
        reader.read_exact(&mut raw)?;
        let mut units = Vec::with_capacity(unit_count);
        for chunk in raw.chunks_exact(2) {
            units.push(u16::from_le_bytes([chunk[0], chunk[1]]));
        }
        if units.last() == Some(&0) {
            units.pop();
        }
        paths.push(String::from_utf16_lossy(&units));
    }
    Ok(paths)
}

fn read_package_header<E, R>(reader: &mut R) -> io::Result<PackageHeader>
where
    R: LinRead,
    E: ByteOrder,
{
    let tag = reader.read_u32::<E>()?;
    read_package_header_after_tag::<E, _>(reader, tag)
}

fn read_package_header_after_tag<E, R>(reader: &mut R, tag: u32) -> io::Result<PackageHeader>
where
    R: LinRead,
    E: ByteOrder,
{
    if tag != PKG_TAG {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "Invalid linker tag {tag:#X} (expected {PKG_TAG:#X}, source_consumed={:#X}, trace_ops_consumed={})",
                reader.source_consumed(),
                reader.trace_ops_consumed()
            ),
        ));
    }

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

    // Mirror SC's `serialize_package_header` (xbe `0x39830`) gate at
    // `0x398c0`: `if (Version_upper16 < 0xc) skip_unknown_block`.
    // The `unk` u32 (= `0xff0adde`) and following packed_int /
    // byte-array are read only when licensee >= 0xc. Most SC packages
    // ship at licensee=0x11 so they take the fast path; a handful of
    // older level-specific texture packages (e.g.
    // `3-3MiningTown_tex` referenced from
    // `5_1_1_PresidentialPalace.lin`) ship at licensee=0xa and
    // legitimately omit the block. Reading it for those misaligns
    // the cursor by exactly 5 bytes (`unk` + packed_int(0)) and the
    // surrounding cascade explodes a few packages later.
    let licensee_version = (version >> 16) as u16;
    let (unk, unknown_data) = if licensee_version >= 0xc {
        let unk = reader.read_u32::<E>()?;
        println!("Unknown value: {:#X}", unk);
        let unknown_data = crate::reader::UnrealReadExt::read_array(reader)?;
        (unk, unknown_data)
    } else {
        (0, Vec::new())
    };

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
    read_package_tables::<E, _>(reader, header)
}

pub fn read_package_for_game<E, R>(reader: &mut R, game: Game) -> io::Result<(RawPackage, u64)>
where
    R: LinRead,
    E: ByteOrder,
{
    let mut source_start = reader.source_consumed();
    let header = if game == Game::PandoraTomorrow {
        let tag_or_count = reader.read_u32::<E>()?;
        if tag_or_count == PKG_TAG {
            read_package_header_after_tag::<E, _>(reader, tag_or_count)?
        } else {
            let paths = read_pt_open_path_list_entries::<E, _>(reader, tag_or_count)?;
            tracing::debug!(?paths, "skipped PT package open path list");
            source_start = reader.source_consumed();
            read_package_header::<E, _>(reader)?
        }
    } else {
        read_package_header::<E, _>(reader)?
    };

    let package = read_package_tables::<E, _>(reader, header)?;
    Ok((package, source_start))
}

fn read_package_tables<E, R>(reader: &mut R, header: PackageHeader) -> io::Result<RawPackage>
where
    R: LinRead,
    E: ByteOrder,
{
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

/// Decompress a `.lin` file and return its decompressed body
/// truncated to `uncompressed_data_size` -- the LIN format's declared
/// content length stored in the first metadata zlib block. Bytes past
/// that point are zlib block alignment padding (consistently ending in
/// a `0xb3` sentinel) that the engine never consumes, and that, if
/// included, corrupt cross-source continuation reads. (009_ChineseEmbassy's
/// variant common.lin is the canary: its trailing 6-byte alignment chunk
/// would shift session.lin's first PKG_TAG by one byte if read.)
pub fn decompress_linear_file<E, R>(reader: &mut R) -> io::Result<Vec<u8>>
where
    R: Read,
    E: ByteOrder,
{
    decompress_linear_file_with_info::<E, _>(reader).map(|info| info.data)
}

pub struct DecompressedLinearFile {
    pub data: Vec<u8>,
    pub declared_size: u32,
    pub game: Game,
    pub metadata: Vec<u32>,
}

/// Decompress a `.lin` and return both the bytes and the
/// `uncompressed_data_size` declared in metadata block 0. Unchecked
/// mode uses the declared size as each source's effective end so the
/// LinReader's auto-advance lands on session.lin's first byte cleanly,
/// without consuming the 0..6 byte zlib alignment tail (which always
/// ends in a `0xb3` sentinel).
pub fn decompress_linear_file_with_size<E, R>(reader: &mut R) -> io::Result<(Vec<u8>, u32)>
where
    R: Read,
    E: ByteOrder,
{
    let info = decompress_linear_file_with_info::<E, _>(reader)?;
    Ok((info.data, info.declared_size))
}

/// Decompress a `.lin` and preserve enough wrapper metadata to identify
/// game-specific LIN dialects. SC1 stores four 4-byte metadata zlib
/// blocks before archive data; Pandora Tomorrow map LINs store five.
/// The engine treats all of these as wrapper fields, not archive bytes.
pub fn decompress_linear_file_with_info<E, R>(reader: &mut R) -> io::Result<DecompressedLinearFile>
where
    R: Read,
    E: ByteOrder,
{
    let mut out_data = Vec::new();
    let mut metadata = Vec::new();

    loop {
        let block = match read_block::<E, _>(reader) {
            Ok(b) => b,
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e),
        };
        let mut zr = ZlibDecoder::new(block.compressed_data.as_slice());
        let mut decoded = Vec::with_capacity(block.uncompressed_len as usize);
        std::io::copy(&mut zr, &mut decoded)?;
        if out_data.is_empty() && block.uncompressed_len == 4 && decoded.len() == 4 {
            metadata.push(u32::from_le_bytes(
                decoded
                    .as_slice()
                    .try_into()
                    .expect("4-byte metadata block"),
            ));
        } else {
            out_data.extend_from_slice(&decoded);
        }
    }

    let declared_size = metadata.first().copied().unwrap_or(out_data.len() as u32);
    let game = match metadata.len() {
        5.. => Game::PandoraTomorrow,
        _ => Game::SplinterCell,
    };

    Ok(DecompressedLinearFile {
        data: out_data,
        declared_size,
        game,
        metadata,
    })
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

fn is_lin_path(path: &str) -> bool {
    path.rsplit(['\\', '/'])
        .next()
        .and_then(|leaf| leaf.rsplit_once('.').map(|(_, ext)| ext))
        .map(|ext| ext.eq_ignore_ascii_case("lin"))
        .unwrap_or(false)
}

fn normalize_checked_io_source_ids(metadata: &mut ExportedData, game: Game) {
    if game != Game::PandoraTomorrow {
        return;
    }

    let mut lin_file_ptrs = std::collections::HashSet::new();
    for open in &metadata.archive_opens {
        if is_lin_path(&open.filename) {
            lin_file_ptrs.insert(open.archive_ptr);
        }
    }

    if lin_file_ptrs.is_empty() {
        return;
    }

    for op in &mut metadata.raw_io_ops {
        match op {
            IoOp::Read { file_ptr, .. } | IoOp::Seek { file_ptr, .. }
                if lin_file_ptrs.contains(file_ptr) =>
            {
                *file_ptr = 0;
            }
            _ => {}
        }
    }

    metadata
        .file_ptr_order
        .retain(|file_ptr| !lin_file_ptrs.contains(file_ptr));
}

/// One `.lin` input to `LinearFileDecoder::new_unchecked`. Bundles the
/// reader with the engine's logical end-of-data so the decoder can cap
/// `LinReader` exactly at the LIN format's `uncompressed_data_size` --
/// the declared size from metadata block 0, not the decompressed
/// buffer's `len()`. Decompressed `.lin` buffers run a few bytes past
/// the engine's EOF (zlib alignment padding consistently ending in a
/// `0xb3` sentinel); reading those bytes shifts subsequent cross-source
/// reads by 1..31 bytes and surfaces as `Invalid linker tag` panics on
/// the 009_ChineseEmbassy variant.
pub struct LinSource<R> {
    pub reader: R,
    pub declared_size: u64,
}

impl<R> LinSource<R> {
    pub fn new(reader: R, declared_size: u64) -> Self {
        Self {
            reader,
            declared_size,
        }
    }
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
    /// package name to `secondary_package_names`. Each `LinSource`'s
    /// `declared_size` caps the corresponding `LinReader`.
    pub fn new_unchecked(sources: Vec<LinSource<R>>) -> Self {
        Self::new_unchecked_for_game(sources, Game::SplinterCell)
    }

    pub fn new_unchecked_for_game(sources: Vec<LinSource<R>>, game: Game) -> Self {
        let mut readers: Vec<R> = Vec::with_capacity(sources.len());
        let mut declared_sizes: Vec<u64> = Vec::with_capacity(sources.len());
        for src in sources {
            readers.push(src.reader);
            declared_sizes.push(src.declared_size);
        }
        let mut secondary_package_names = Vec::with_capacity(readers.len().saturating_sub(1));
        let mut prefix_lengths: Vec<u64> = vec![0; readers.len()];
        for (i, source) in readers.iter_mut().enumerate().skip(1) {
            let (name, prefix_len) = skip_secondary_lin_header::<_, E>(source, game)
                .expect("failed to skip secondary lin header");
            secondary_package_names.push(name);
            prefix_lengths[i] = prefix_len;
        }
        let mut combined = LinReader::new_multi(readers);
        // Seed each secondary's `source_consumed_per_source` to the
        // post-prefix Cursor position. The prefix bytes were consumed
        // by `skip_secondary_lin_header` directly on the Cursor (not
        // through the LinReader), so without this seeding the read /
        // physical-position bookkeeping would be off by `prefix_len`
        // bytes -- surfacing as a wrong `pkg_source_start` when a
        // package on this source is loaded.
        for (i, &len) in prefix_lengths.iter().enumerate() {
            combined.seed_source_consumed(i, len);
        }
        for (i, &size) in declared_sizes.iter().enumerate() {
            combined.set_source_size_limit(i, Some(size));
        }
        Self {
            sources: VecDeque::from(vec![combined]),
            runtime: UnrealRuntime {
                game,
                linkers: HashMap::new(),
                objects_full_loading: Default::default(),
                loaded_objects: Default::default(),
                package_file_size: HashMap::new(),
                // present_packages stays empty until `read_lin_header`
                // populates it from the file_table; we don't have a
                // recorded `file_load_order` to seed from.
                present_packages: Default::default(),
                engine_constructed_objects: None,
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
    pub fn new_checked(sources: Vec<R>, metadata: ExportedData) -> Self {
        Self::new_checked_for_game(sources, metadata, Game::SplinterCell)
    }

    pub fn new_checked_for_game(
        mut sources: Vec<R>,
        mut metadata: ExportedData,
        game: Game,
    ) -> Self {
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
            let (name, _prefix_len) = skip_secondary_lin_header::<_, E>(source, game)
                .expect("failed to skip secondary lin header");
            secondary_package_names.push(name);
        }
        normalize_checked_io_source_ids(&mut metadata, game);
        let expected_source_per_op = compute_expected_source_per_op(&metadata);
        let validate_seeks = metadata
            .raw_io_ops
            .iter()
            .any(|op| matches!(op, IoOp::Seek { .. }));
        let io_ops = Rc::new(RefCell::new(metadata.raw_io_ops.drain(..).collect()));
        let file_ptr_order = metadata.file_ptr_order.clone();
        // Single CheckedLinReader holds all sources and switches between
        // them based on each op's `file_ptr`. The trace's
        // `file_ptr_order` (already reversed by bin.rs to consumption
        // order) tells us which source each new file_ptr value maps to.
        let mut combined = CheckedLinReader::new(sources, file_ptr_order, io_ops);
        combined.set_validate_seeks(validate_seeks);
        combined.expected_source_per_op = expected_source_per_op;
        Self {
            sources: VecDeque::from(vec![combined]),
            runtime: UnrealRuntime {
                game,
                linkers: HashMap::with_capacity(metadata.file_load_order.len()),
                objects_full_loading: Default::default(),
                loaded_objects: Default::default(),
                package_file_size: HashMap::new(),
                present_packages: metadata
                    .file_load_order
                    .iter()
                    .filter_map(|p| package_name_from_path(p).map(|n| n.to_lowercase()))
                    .collect(),
                engine_constructed_objects: (game == Game::SplinterCellPrototype)
                    .then(|| metadata.gobj_loaded_order.iter().cloned().collect()),
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

    /// Map from lowercased package name (the linker key style -- `"engine"`,
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
            // walks up one level -- but be defensive in case any entry has
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
        // `read_lin_header` already populated `present_packages` from
        // common.lin's `file_table` -- every package that ships in
        // common.lin and every secondary `.lin` (Maps/, StaticMeshes/,
        // Animations/, Sounds/, System/) is named there. Mirrors how
        // SC's `GetPackageLinker` (xbe `0x39da0`) resolves package
        // names against the underlying file system + LinkerCache --
        // file_table membership IS the engine's "this package
        // physically exists" signal.
        //
        // (The previous hardcoded `COMMON_LIN_PACKAGES` list and the
        // `discover_secondary_package_names` PKG_TAG window scan
        // were both subsets of what the file_table already provides;
        // removed in favour of trusting the engine-faithful source.)

        // Stage 1: replay the pre-MyLevel warmup against source 0. Each
        // call is idempotent -- duplicates that the cascade already
        // pulled in just no-op via `runtime.loaded_objects`.
        //
        // Tolerate `UnexpectedEof`: the 009_ChineseEmbassy variant's
        // common.lin is a 3.1 MB prefix of the dominant 6.6 MB file
        // (verified byte-identical for that prefix), so the cascade
        // hits source EOF on packages whose bodies are only present
        // in the dominant build. `verify_imports` already swallows EOF
        // mid-cascade (returns Ok), but a top-level warmup call can
        // still surface EOF when a body Preload reads past cap. In
        // that case we log and continue to the next warmup item; the
        // already-loaded linkers stay populated and Stage 2 picks up
        // the remainder from session.lin.
        for object in crate::engine_warmup::ENGINE_CLASS_WARMUP {
            let reader = self.sources.front_mut().expect("no file reader available?");
            self.runtime.begin_load();
            let load_res = self.runtime.load_object_by_full_name::<E, _>(
                object,
                crate::runtime::LoadKind::Load,
                reader,
            );
            let drain_res = self.runtime.end_load::<E, _>(reader);
            match (load_res, drain_res) {
                (Ok(_), Ok(())) => {}
                (Err(e), _) | (_, Err(e)) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    tracing::warn!("Stage 1 warmup hit EOF at {object}: {e}; continuing");
                }
                (Err(e), _) => return Err(e),
                (Ok(_), Err(e)) => return Err(e),
            }
        }

        // Stage 2: load `<secondary>.MyLevel` from source 1. The
        // ULevel cascade pulls in the post-MyLevel objects the menu
        // trace records (game info, HUD, controller, etc.) via actor
        // class refs.
        //
        // Class filter is `Engine.Level` (`ULevel::StaticClass`) -- SC's
        // `LoadMap` (xbe `0x81860`) calls
        // `StaticLoadObject(Engine.Level, MyLevel, FileName, ...)`,
        // and that filter is load-bearing for maps that ship two
        // `MyLevel` exports: `2_1_0CIA` has both `class=Level`
        // (the real ULevel with all actor children) and `class=Package`
        // (a wrapper container). Without the type filter our resolver
        // could land on the Package export and miss the level cascade.
        let reader = self.sources.front_mut().expect("no file reader available?");
        let secondary = self
            .secondary_package_names
            .first()
            .cloned()
            .expect("unchecked decode requires a secondary .lin source");
        reader.switch_to_source(1);
        let target = format!("{secondary}.MyLevel");
        self.runtime.begin_load();
        self.runtime.load_object_by_full_name_with_class::<E, _>(
            &target,
            Some(("Level", "Engine")),
            crate::runtime::LoadKind::Load,
            reader,
        )?;
        self.runtime.end_load::<E, _>(reader)?;

        // Post-MyLevel script-driven asset loads. The cascade above
        // covers everything reachable via property-tag refs from MyLevel,
        // but the engine's runtime continues after `LoadMap` by executing
        // each actor's PreBeginPlay/BeginPlay/PostBeginPlay UnrealScript
        // bytecode. Those scripts call `StaticLoadObject(class'X', "Y")`
        // with literal asset names -- which the LIN compactor laid out
        // immediately after the cascade's last byte, in the engine's
        // exact call order. Walking the parsed bytecode of every actor
        // class's init functions and triggering each `StringConst`
        // payload as a load advances our cursor in lockstep.
        let reader = self.sources.front_mut().expect("no file reader available?");
        crate::object::internal::post_cascade::run_post_cascade::<E, _>(
            &secondary,
            &mut self.runtime,
            reader,
        )?;

        // Stage 3: replay the post-MyLevel engine class loads. The
        // engine's `LoadMap` (xbe `0x81860`) triggers these via
        // GameInfo/PlayerController/Pawn spawn + their script chains
        // running after the actor BeginPlay loop. We can't simulate
        // the spawn end-to-end, but the explicit class-umbrella loads
        // here cover the same byte ranges the engine reads at this
        // phase (verified per-map against recorded traces:
        // `EchelonCharacter.ESam (Class)` umbrella + its CDO sound
        // refs sit at the end of session.lin, and our cascade
        // otherwise stops at `ENPC.proAnims`).
        let reader = self.sources.front_mut().expect("no file reader available?");
        for full_name in crate::engine_warmup::POST_LEVEL_LOAD_LIST {
            self.runtime.begin_load();
            let load_res = self.runtime.load_object_by_full_name::<E, _>(
                full_name,
                crate::runtime::LoadKind::Load,
                reader,
            );
            let drain_res = self.runtime.end_load::<E, _>(reader);
            match (load_res, drain_res) {
                (Ok(_), Ok(())) => {}
                (Err(e), _) | (_, Err(e)) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    tracing::warn!(
                        "Stage 3 post-level load hit EOF at {full_name}: {e}; continuing"
                    );
                }
                (Err(e), _) => {
                    tracing::warn!(
                        "Stage 3 post-level load failed for {full_name}: {e}; continuing"
                    );
                }
                (Ok(_), Err(e)) => {
                    tracing::warn!("Stage 3 drain after {full_name} failed: {e}; continuing");
                }
            }
        }

        // Stage 4: post-spawn actor-driven sound preloads. SC's xbe
        // `sub_271e0` runs after `LoadMap`'s actor BeginPlay loop and
        // iterates the level's actors a second time, calling
        // `StaticLoadObject(Engine.Sound, "<pkg>.<name><level_suffix>")`
        // for each actor that matches one of five `IsA` tests
        // (EPawn / EWeapon / ESensor / EchelonLevelInfo /
        // StaticMeshActor). This populates the trailing `Engine`
        // sub-package at session.lin's tail -- the last 789 bytes our
        // post-MyLevel cascade otherwise misses.
        let reader = self.sources.front_mut().expect("no file reader available?");
        crate::object::internal::post_cascade::run_post_spawn_actor_loads::<E, _>(
            &secondary,
            &mut self.runtime,
            reader,
        )?;

        Ok(())
    }

    pub fn decode_linear_file(&mut self) -> io::Result<()> {
        self.read_lin_header()?;

        for object in &self.metadata.object_load_order {
            let reader = self.sources.front_mut().expect("no file reader available?");
            println!("Loading {object}");
            // `None.MyLevel` is the engine's bootstrap marker for the
            // level-package load. SC's `LoadMap` (xbe `0x81860`)
            // resolves it via `StaticLoadObject(Engine.Level,
            // MyLevel, FileName, ...)` where FileName is the per-region
            // map subdir (typically the same as the secondary `.lin`'s
            // package name: `"menu"` for menu.lin, `"0_0_2_Training"`
            // for training, etc.). The level package lives in a
            // separate `.lin` source, so we explicitly switch to source
            // 1 and then invoke the resolver with the engine's class
            // filter (`Engine.Level` = `ULevel::StaticClass`). The
            // class filter matters for maps that ship two `MyLevel`
            // exports -- see comment in `decode_unchecked`.
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
            if resolved.is_some() {
                self.runtime.load_object_by_full_name_with_class::<E, _>(
                    target,
                    Some(("Level", "Engine")),
                    crate::runtime::LoadKind::Load,
                    reader,
                )?;
            } else {
                self.runtime.load_object_by_full_name::<E, _>(
                    target,
                    crate::runtime::LoadKind::Load,
                    reader,
                )?;
            }
            self.runtime.end_load::<E, _>(reader)?;
        }

        Ok(())
    }

    pub fn read_lin_header(&mut self) -> io::Result<()> {
        let has_file_table = !self.file_table.is_empty();
        let game = self.runtime.game;

        let reader = self.reader();

        reader.set_reading_linker_header(true);

        match game {
            // Retail/demo SC1 and the proto share the LIN header
            // shape (u32 load_address + length-prefixed name); the
            // proto-specific differences kick in at the per-export
            // native serialize layer, not here.
            Game::SplinterCell | Game::SplinterCellPrototype => {
                let _unk = reader.read_u32::<E>()?;
                let _name = reader.read_string()?;
            }
            Game::PandoraTomorrow => {
                let _name = reader.read_string()?;
            }
        }

        if has_file_table {
            reader.set_reading_linker_header(false);
            return Ok(());
        }

        let tag = reader.read_u32::<E>()?;
        assert_eq!(tag, LIN_FILE_TABLE_TAG, "LIN file table tag mismatch");

        let file_table = read_file_table::<E, _>(reader).expect("failed to read file table");
        if game == Game::PandoraTomorrow {
            let _unknown = reader.read_u32::<E>()?;
            let paths = read_pt_open_path_list::<E, _>(reader)
                .expect("failed to read PT LIN open path list");
            tracing::debug!(?paths, "read PT LIN open path list");
        }
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
                // and skipped -- leaving its header bytes in the source
                // unread, which then misaligns subsequent reads.
                //
                // Stored lowercase to match UE2 FName case-insensitive
                // equality (file_table has e.g. `Sounds\Water.uax` but
                // package imports reference it as `water`).
                self.runtime.present_packages.insert(pkg.to_lowercase());
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

/// Try to parse a UE2 package header starting at `data[offset]`.
/// Returns `None` if the bytes don't look like a valid SC package
/// header (wrong version, out-of-bounds offsets, etc.). Used by the
/// merge-time `scan_tail_packages` diagnostic to label unread-tail
/// PKG_TAG byte matches with the package's first non-`None` name --
/// purely for warning messages, never for cascade-routing decisions
/// (those go through common.lin's file_table).
pub fn try_parse_package_at<E: ByteOrder>(data: &[u8], offset: usize) -> Option<RawPackage> {
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

    if reader
        .seek(SeekFrom::Start(header.export_offset as u64))
        .is_err()
    {
        return Some(RawPackage {
            header,
            names,
            imports,
            exports: Vec::new(),
        });
    }
    let mut exports = Vec::with_capacity(header.export_count as usize);
    for _ in 0..header.export_count as usize {
        match read_export::<E, _>(&mut reader) {
            Ok(e) => exports.push(e),
            Err(_) => break,
        }
    }

    Some(RawPackage {
        header,
        names,
        imports,
        exports,
    })
}

/// Read a UE packed int directly from a raw byte stream.
fn read_packed_int_from_read<R>(source: &mut R, bytes_read: &mut u64) -> io::Result<i32>
where
    R: Read,
{
    let mut byte = [0u8; 1];
    source.read_exact(&mut byte)?;
    *bytes_read += 1;
    let b0 = byte[0];
    let mut value: u32 = 0;
    if (b0 & 0x40) != 0 {
        source.read_exact(&mut byte)?;
        *bytes_read += 1;
        let b1 = byte[0];
        if (b1 & 0x80) != 0 {
            source.read_exact(&mut byte)?;
            *bytes_read += 1;
            let b2 = byte[0];
            if (b2 & 0x80) != 0 {
                source.read_exact(&mut byte)?;
                *bytes_read += 1;
                let b3 = byte[0];
                if (b3 & 0x80) != 0 {
                    source.read_exact(&mut byte)?;
                    *bytes_read += 1;
                    value = byte[0] as u32;
                }
                value = (value << 7) + ((b3 & 0x7f) as u32);
            }
            value = (value << 7) + ((b2 & 0x7f) as u32);
        }
        value = (value << 7) + ((b1 & 0x7f) as u32);
    }
    value = (value << 6) + ((b0 & 0x3f) as u32);
    let mut result = value as i32;
    if (b0 & 0x80) != 0 {
        result = -result;
    }
    Ok(result)
}

fn read_raw_ansi_string<R>(source: &mut R, bytes_read: &mut u64) -> io::Result<String>
where
    R: Read,
{
    let value = read_packed_int_from_read(source, bytes_read)?;
    if value < 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "secondary LIN header name was unexpectedly UTF-16",
        ));
    }
    let mut name_buf = vec![0u8; value as usize];
    source.read_exact(&mut name_buf)?;
    *bytes_read += value as u64;
    if name_buf.last() == Some(&0) {
        name_buf.pop();
    }
    Ok(String::from_utf8_lossy(&name_buf).into_owned())
}

/// Read past a secondary `.lin` source's LIN-format prefix. SC1 uses
/// `u32 load_address + packed ANSI name`; PT stores only the packed
/// ANSI name, then a small counted UTF-16 open-path list before the
/// first package.
fn skip_secondary_lin_header<R, E>(source: &mut R, game: Game) -> io::Result<(String, u64)>
where
    R: Read,
    E: ByteOrder,
{
    let mut bytes_read: u64 = 0;
    if matches!(game, Game::SplinterCell | Game::SplinterCellPrototype) {
        let mut tmp = [0u8; 4];
        source.read_exact(&mut tmp)?; // load_address
        bytes_read += 4;
    }

    let name = read_raw_ansi_string(source, &mut bytes_read)?;

    if game == Game::PandoraTomorrow {
        let _unknown = source.read_u32::<E>()?;
        bytes_read += 4;
        let before_paths = bytes_read;
        let paths = read_pt_open_path_list::<E, _>(source)?;
        let path_bytes = paths
            .iter()
            .map(|path| 4 + ((path.encode_utf16().count() + 1) * 2) as u64)
            .sum::<u64>()
            + 4;
        bytes_read = before_paths + path_bytes;
        tracing::debug!(?paths, "skipped PT secondary LIN open path list");
    }

    Ok((name, bytes_read))
}

fn package_name_from_path(path: &str) -> Option<String> {
    let leaf = path.rsplit('\\').next().unwrap_or(path);
    let stem = leaf.rsplit_once('.').map(|(s, _)| s).unwrap_or(leaf);
    if stem.is_empty() {
        None
    } else {
        Some(stem.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Locks down the flat JSON wire format produced by the QEMU plugin
    /// (`reads.json`): all 7 export fields appear at the top level of
    /// the object. `#[serde(flatten)]` on `ObjectExport::fields` must
    /// preserve that shape so existing trace files keep loading.
    #[test]
    fn object_export_round_trips_flat_json() {
        let json = br#"{"class_index":-1,"super_index":0,"package_index":2,"object_name":42,"object_flags":99,"serial_size":100,"serial_offset":2000}"#;
        let e: ObjectExport = serde_json::from_slice(json).expect("flat-json deserialize");
        assert_eq!(e.fields.class_index, -1);
        assert_eq!(e.fields.super_index, 0);
        assert_eq!(e.fields.package_index, 2);
        assert_eq!(e.fields.object_name, 42);
        assert_eq!(e.fields.object_flags, 99);
        assert_eq!(e.serial_size, 100);
        assert_eq!(e.serial_offset, 2000);

        // Deref makes the inner fields accessible without `.fields.`,
        // so historical call-site shape keeps working.
        assert_eq!(e.class_index, -1);
        assert_eq!(e.object_name, 42);
    }

    #[test]
    fn package_name_from_path_strips_backslash_dirs_and_extension() {
        assert_eq!(
            package_name_from_path("System\\Engine.u").as_deref(),
            Some("Engine")
        );
        assert_eq!(
            package_name_from_path("Maps\\menu\\menu.unr").as_deref(),
            Some("menu")
        );
        assert_eq!(
            package_name_from_path("Sounds\\Water.uax").as_deref(),
            Some("Water")
        );
        assert_eq!(
            package_name_from_path("Animations\\ENPC.ukx").as_deref(),
            Some("ENPC")
        );
    }

    #[test]
    fn package_name_from_path_empty_returns_none() {
        assert_eq!(package_name_from_path(""), None);
    }

    /// Regression test for the case-sensitivity bug fixed by
    /// recommendation 3 of the audit. The file_table ships
    /// `Sounds\Water.uax` (capital W) but the package's imports
    /// reference it as `water` (lowercase). UE2 FName equality is
    /// case-insensitive; our `present_packages` HashSet must mirror
    /// that or the cascade misaligns. Concretely: this lookup must
    /// succeed for the engine-faithful gate to admit the package.
    #[test]
    fn present_packages_lookup_matches_engine_fname_case_insensitivity() {
        use std::collections::HashSet;
        let mut present: HashSet<String> = HashSet::new();
        // Names from common.lin's file_table go in lowercased.
        for entry in [
            "System\\Engine.u",
            "Sounds\\Water.uax",
            "Animations\\ENPC.ukx",
            "Maps\\menu\\menu.unr",
        ] {
            if let Some(p) = package_name_from_path(entry) {
                present.insert(p.to_lowercase());
            }
        }
        // Lookups (whatever case the import uses) must succeed.
        for import_name in ["Engine", "engine", "water", "Water", "ENPC", "enpc", "menu"] {
            assert!(
                present.contains(&import_name.to_lowercase()),
                "import {import_name:?} should resolve in present_packages",
            );
        }
    }
}
