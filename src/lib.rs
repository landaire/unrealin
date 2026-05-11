// Many object deserialisers record fields purely for RE
// documentation -- the bytes appear on disk and we want them named
// even when no caller reads them yet. Same for LoadKind::Full and a
// handful of array offsets like ARRAY_B_OFFSET_AVG_BYTES_PER_SEC.
// Crate-wide dead_code allow keeps the noise out of clippy.
#![allow(dead_code)]

pub mod audio;
pub mod de;
pub mod diag;
pub mod merge;
pub mod ser;

pub(crate) mod common;
pub(crate) mod engine_warmup;
pub(crate) mod object;
pub(crate) mod reader;
pub(crate) mod runtime;

pub(crate) const PKG_TAG: u32 = 0x9e2a83c1;
pub(crate) const LIN_FILE_TABLE_TAG: u32 = 0x9FE3C5A3;

pub use common::ExportedData;
