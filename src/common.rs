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
}

#[derive(Debug, Deserialize)]
pub enum IoOp {
    Seek {
        to: u64,
        from: u64,
        #[serde(default)]
        file_ptr: u32,
    },
    Read {
        len: u64,
        #[serde(default)]
        file_ptr: u32,
    },
}
