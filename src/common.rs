use std::collections::HashMap;

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
    pub export: ObjectExport<'static>,
    pub len: usize,
    pub ignore: bool,
    pub start_offset: u64,
}

#[derive(Deserialize)]
pub struct ExportedData {
    pub file_load_order: Vec<String>,
    pub file_reads: HashMap<u32, Vec<ExportRead>>,
    pub file_ptr_order: Vec<u32>,
}
