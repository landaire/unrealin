use std::collections::{HashMap, VecDeque};

use serde::{Deserialize, Serialize};

use crate::de::ObjectExport;

pub fn normalize_index(index: i32) -> usize {
    match index {
        i if i < 0 => (-index) as usize - 1,
        i if i > 0 => index as usize - 1,
        _ => 0,
    }
}

#[derive(Debug, Deserialize)]
pub struct ExportRead {
    pub export: ObjectExport,
    pub len: usize,
    pub ignore: bool,
    pub start_offset: u64,
}

#[derive(Default, Deserialize)]
pub struct ExportedData {
    pub file_load_order: Vec<String>,
    /// Trace-op index at which each file in `file_load_order` had its
    /// `ULinkerLoad` ctor entered. Index 0 means "before the first
    /// trace op was recorded" (Engine.u, Core.u typically). Used by
    /// `CheckedLinReader::expected_source_per_op` to verify our load
    /// timing matches the engine's.
    #[serde(default)]
    pub file_load_op_index: Vec<usize>,
    pub file_reads: HashMap<u32, Vec<ExportRead>>,
    pub file_ptr_order: Vec<u32>,
    pub raw_io_ops: Vec<IoOp>,
    pub object_load_order: Vec<String>,
    #[serde(default)]
    pub gobj_loaded_order: Vec<String>,
    /// Per-`CreateFileReader` open record from the QEMU plugin. Each
    /// entry pairs an FArchive heap address (`archive_ptr`) with the
    /// filename the engine called `CreateFileReader` with, captured at
    /// the call site of either `WindowsFileReader::WindowsFileReader`
    /// or `CompressedFileReader::CompressedFileReader`. New opens
    /// append; the LAST entry matching a given `archive_ptr` is the
    /// active open, so heap-address reuse is unambiguous.
    #[serde(default)]
    pub archive_opens: Vec<ArchiveOpen>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ArchiveOpen {
    pub archive_ptr: u32,
    pub filename: String,
    pub op_index: usize,
}

#[derive(Debug, Deserialize, Clone)]
pub enum IoOp {
    Seek {
        to: u64,
        from: u64,
        #[serde(default)]
        file_ptr: u32,
        /// Index into `ExportedData::archive_opens` identifying which
        /// CreateFileReader return this op belongs to. -1 if no
        /// matching open is recorded (early ops before the first
        /// hook fired).
        #[serde(default = "neg_one_i64")]
        archive_open: i64,
        /// FArchive heap address (`this` at the seek hook) the engine's
        /// `Seek` was called on. Disambiguates seeks issued on
        /// different FArchive instances that share an FFile* (and
        /// therefore the same `file_ptr`).
        #[serde(default)]
        seek_archive: u32,
    },
    Read {
        len: u64,
        #[serde(default)]
        file_ptr: u32,
        #[serde(default = "neg_one_i64")]
        archive_open: i64,
        /// FArchive's `+0x40` position field captured BEFORE the read
        /// advances it. 0 if the trace predates this instrumentation.
        #[serde(default)]
        archive_pos: u64,
    },
}

fn neg_one_i64() -> i64 {
    -1
}
