use std::{
    array,
    cell::RefCell,
    collections::{BTreeMap, HashMap, VecDeque},
    io::{self, Read, Seek},
    rc::Rc,
};

use byteorder::{ByteOrder, ReadBytesExt};
use tracing::{Level, debug, span, trace};

use crate::{
    common::IoOp,
    de::{ExportIndex, ImportIndex, Linker, RcLinker},
    object::{DeserializeUnrealObject, RcUnrealObject, UnrealObject},
    runtime::{LoadKind, UnrealRuntime},
};

pub trait UnrealReadExt: LinRead + Sized {
    fn read_object<E>(
        &mut self,
        runtime: &mut UnrealRuntime,
        linker: &RcLinker,
    ) -> io::Result<Option<RcUnrealObject>>
    where
        E: ByteOrder,
    {
        let span = span!(Level::DEBUG, "read_object");
        let _enter = span.enter();

        let pos = self.stream_position()?;
        let index = self.read_packed_int()?;
        let after = self.stream_position()?;

        trace!("Read {} bytes (obj_index= {:#X})", after - pos, index);

        runtime.load_object_by_raw_index::<E, _>(index, linker, LoadKind::Create, self)
    }

    /// Decodes the packed integer from the byte stream.
    /// Assumes `u8(input)` reads one byte from `input`.
    fn read_packed_int(&mut self) -> io::Result<i32> {
        const CONTINUE_BIT: u8 = 0x40;
        const NEGATE_BIT: u8 = 0x80;

        let span = span!(Level::TRACE, "read_packed_int");
        let _enter = span.enter();

        let b0 = self.read_u8()?;

        trace!("b0: {:#X}", b0);

        // Build up the unsigned magnitude.
        let mut value: u32 = 0;

        if (b0 & CONTINUE_BIT) != 0 {
            let b1 = self.read_u8()?;
            trace!("b1: {b1:#X}");
            if (b1 & NEGATE_BIT) != 0 {
                let b2 = self.read_u8()?;
                trace!("b2: {b2:#X}");
                if (b2 & NEGATE_BIT) != 0 {
                    let b3 = self.read_u8()?;
                    trace!("b3: {b3:#X}");
                    if (b3 & NEGATE_BIT) != 0 {
                        let b4 = self.read_u8()?;
                        trace!("b4: {b4:#X}");
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

    fn read_array(&mut self) -> io::Result<Vec<u8>> {
        let array_len = self.read_packed_int()?;
        assert!(array_len >= 0, "Packed array length is negative");

        let mut data = vec![0u8; array_len as usize];
        self.read_exact(&mut data)?;

        Ok(data)
    }

    fn read_serializable_array<E, T>(
        &mut self,
        runtime: &mut UnrealRuntime,
        linker: &RcLinker,
    ) -> io::Result<Vec<T>>
    where
        T: DeserializeUnrealObject + Default + Clone,
        E: ByteOrder,
    {
        let span = span!(Level::TRACE, "read_packed_int_array");
        let _enter = span.enter();

        let array_len = self.read_packed_int()?;
        assert!(array_len >= 0, "Packed array length is negative");

        debug!("Array len: {array_len:#X}");

        let mut data = vec![T::default(); array_len as usize];
        for obj in &mut data {
            obj.deserialize::<E, _>(runtime, linker, self)?;
        }

        Ok(data)
    }

    fn read_string(&mut self) -> io::Result<String> {
        let string_len = self.read_packed_int()?;

        if string_len == 0 {
            return Ok(String::new());
        }

        let is_unicode = string_len < 0;
        let actual_len = string_len.unsigned_abs() as usize;

        if is_unicode {
            // UE2 unicode strings are UTF-16 LE wide chars, null-terminated.
            // The packed_int magnitude is the wide-char count *including*
            // the trailing null. Some training-map FURL strings hit this
            // path; without it, read_string panics mid-Level deserialize.
            let mut units: Vec<u16> = Vec::with_capacity(actual_len);
            for _ in 0..actual_len {
                let lo = self.read_u8()? as u16;
                let hi = self.read_u8()? as u16;
                units.push(lo | (hi << 8));
            }
            if units.last() == Some(&0) {
                units.pop();
            }
            Ok(String::from_utf16_lossy(&units))
        } else {
            // ANSI strings - read byte by byte. UE2 ANSI strings are Latin-1
            // (e.g. SC has French asset names like `…Defé_GLW`); blindly
            // decoding as UTF-8 panics on the first 0x80-0xFF byte.
            let mut string_data = Vec::with_capacity(actual_len);
            for _ in 0..actual_len {
                string_data.push(self.read_u8()?);
            }

            // Remove the null terminator if present
            if !string_data.is_empty() && string_data[string_data.len() - 1] == 0 {
                string_data.pop();
            }

            Ok(string_data.into_iter().map(|b| b as char).collect())
        }
    }
}

impl<R: LinRead + Sized> UnrealReadExt for R {}

pub struct LinReader<R> {
    /// One source per `.lin` file. The runtime calls `switch_to_source`
    /// at known boundary points (typically when bootstrap reaches
    /// `None.MyLevel`, which lives in the secondary `.lin`). The active
    /// source is `sources[current_source_idx]`.
    sources: Vec<R>,
    current_source_idx: usize,
    pos: u64,
    /// Total bytes consumed across all sources. `source_consumed_per_source`
    /// breaks this down per source for the EOF safety net.
    source_consumed: u64,
    source_consumed_per_source: Vec<u64>,
    /// Stack of capture buffers. Each `read` appends to the top frame.
    /// `push_capture` / `pop_capture` (the LinRead trait) wrap an
    /// export's deserialize so we can extract the exact bytes that
    /// went into its body.
    capture_stack: Vec<Vec<u8>>,
    /// Source-absolute start of the current package within the `.lin`
    /// stream. Package-relative virtual seeks add this to translate to
    /// the absolute source offset. Set by `push_linker` (and
    /// `set_source_start` for the initial `read_package` call before a
    /// `Linker` exists).
    source_start: u64,
    /// Stack of `source_start` values to restore on `pop_linker`.
    source_start_stack: Vec<u64>,
    version: u16,
    linker: Vec<RcLinker>,
}

impl<R> LinReader<R> {
    pub fn new(reader: R) -> Self {
        LinReader::new_multi(vec![reader])
    }

    pub fn new_multi(sources: Vec<R>) -> Self {
        let n = sources.len();
        LinReader {
            sources,
            current_source_idx: 0,
            pos: 0,
            source_consumed: 0,
            source_consumed_per_source: vec![0; n],
            capture_stack: Vec::new(),
            source_start: 0,
            source_start_stack: Vec::new(),
            version: 0,
            linker: Default::default(),
        }
    }

    pub fn source_consumed(&self) -> u64 {
        self.source_consumed
    }

    pub fn source_consumed_per_source(&self) -> &[u64] {
        &self.source_consumed_per_source
    }

    pub fn set_source_start(&mut self, start: u64) {
        self.source_start = start;
    }
}

impl<R> Read for LinReader<R>
where
    R: Read,
{
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let cur = self.current_source_idx;
        let bytes_read = self.sources[cur].read(buf)?;
        self.pos += bytes_read as u64;
        self.source_consumed += bytes_read as u64;
        self.source_consumed_per_source[cur] += bytes_read as u64;
        if let Some(top) = self.capture_stack.last_mut() {
            top.extend_from_slice(&buf[..bytes_read]);
        }
        if let Some(linker) = self.linker.last_mut() {
            linker.borrow_mut().set_position(self.pos);
        }

        Ok(bytes_read)
    }
}

impl<R> Seek for LinReader<R> {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        let res = match pos {
            std::io::SeekFrom::Start(pos) => {
                self.pos = pos;
                Ok(pos)
            }
            std::io::SeekFrom::End(_) => todo!("end position seeking not implemented"),
            std::io::SeekFrom::Current(0) => Ok(self.pos),
            std::io::SeekFrom::Current(_) => todo!("current position seeking not implemented"),
        };

        if let Some(linker) = self.linker.last_mut() {
            linker.borrow_mut().set_position(self.pos);
        }

        res
    }
}

pub struct CheckedLinReader<R> {
    /// One source per `file_ptr` from the recorded IO trace. Each op
    /// carries its own `file_ptr`; we switch to the matching source
    /// before consuming the op. Maps `file_ptr` -> index into `sources`.
    sources: Vec<R>,
    file_ptr_to_index: HashMap<u32, usize>,
    /// Sources that haven't been bound to a file_ptr yet, popped off
    /// in `file_ptr_order` as new file_ptrs appear in the trace.
    file_ptr_order: VecDeque<u32>,
    current_source_idx: usize,
    pos: u64,
    source_consumed: u64,
    capture_stack: Vec<Vec<u8>>,
    source_start: u64,
    source_start_stack: Vec<u64>,
    version: u16,
    /// Counter for nested "in linker header" frames. Incremented by
    /// `set_reading_linker_header(true)`, decremented by `(false)`.
    /// Used as a depth counter so nested `load_linker` calls (e.g. from
    /// VerifyAllImports cascading into other top-level packages) keep the
    /// "skip IO op consumption" mode active across the whole nested span.
    /// Effective state: `linker_header_depth > 0`.
    linker_header_depth: u32,
    /// Counter of trace ops popped (Read or Seek). Used to compute the
    /// trace-op index for diagnostic logging only.
    pub trace_ops_consumed: u64,
    /// Optional per-trace-op expected source position from the engine.
    /// When set, after each trace op pop we assert `source_consumed`
    /// matches `expected_source[trace_ops_consumed]`. First mismatch
    /// pinpoints the divergence.
    pub expected_source_per_op: Option<Vec<u64>>,
    /// Track the previous drift to only log the FIRST occurrence and
    /// any change (avoid flooding output).
    pub last_drift: i64,
    io_ops: Rc<RefCell<VecDeque<IoOp>>>,
    /// Bytes consumed from each source individually. Indexed by
    /// `current_source_idx` so the EOF safety net can verify both
    /// `.lin` files were drained at the end of a run.
    source_consumed_per_source: Vec<u64>,
    linker: Vec<RcLinker>,
}

impl<R> CheckedLinReader<R> {
    pub fn new(
        sources: Vec<R>,
        file_ptr_order: Vec<u32>,
        io_ops: Rc<RefCell<VecDeque<IoOp>>>,
    ) -> Self {
        let mut file_ptr_to_index = HashMap::new();
        // Pre-bind any file_ptrs we already know about (in case the trace
        // never references one — won't matter, but harmless).
        for (i, ptr) in file_ptr_order.iter().enumerate() {
            file_ptr_to_index.insert(*ptr, i);
        }
        let source_count = sources.len();
        CheckedLinReader {
            sources,
            file_ptr_to_index,
            file_ptr_order: VecDeque::from(file_ptr_order),
            current_source_idx: 0,
            pos: 0,
            source_consumed: 0,
            source_consumed_per_source: vec![0; source_count],
            capture_stack: Vec::new(),
            source_start: 0,
            source_start_stack: Vec::new(),
            linker_header_depth: 0,
            trace_ops_consumed: 0,
            expected_source_per_op: None,
            last_drift: 0,
            io_ops,
            version: 0,
            linker: Default::default(),
        }
    }

    pub fn source_consumed(&self) -> u64 {
        self.source_consumed
    }

    pub fn set_source_start(&mut self, start: u64) {
        self.source_start = start;
    }

    pub fn ops_remaining(&self) -> usize {
        self.io_ops.borrow().len()
    }

    pub fn source_consumed_per_source(&self) -> &[u64] {
        &self.source_consumed_per_source
    }

    /// Switch the active source to the one bound to `file_ptr`. Binds
    /// `file_ptr` to the next free source on first encounter (in
    /// recorded `file_ptr_order`).
    fn switch_source(&mut self, file_ptr: u32) {
        if file_ptr == 0 {
            return; // legacy/no-info ops keep current source
        }
        let idx = if let Some(&i) = self.file_ptr_to_index.get(&file_ptr) {
            i
        } else {
            let i = self.file_ptr_to_index.len();
            self.file_ptr_to_index.insert(file_ptr, i);
            i
        };
        if idx < self.sources.len() {
            self.current_source_idx = idx;
        }
    }
}

impl<R> Read for CheckedLinReader<R>
where
    R: Read,
{
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut next_file_ptr: u32 = 0;
        if self.linker_header_depth == 0 {
            let mut ops = self.io_ops.borrow_mut();

            match ops
                .pop_front()
                .expect("conducting an IO op but there are no more IO ops")
            {
                IoOp::Read { len, file_ptr, .. } => {
                    next_file_ptr = file_ptr;
                    let remaining = ops.len();
                    assert_eq!(
                        buf.len() as u64,
                        len,
                        "Expected a read of {:#X} bytes, got read of {:#X} instead (remaining ops: {})",
                        len,
                        buf.len(),
                        remaining
                    );
                    self.trace_ops_consumed += 1;
                }
                other => panic!(
                    "doing a read of {:#X} bytes at {:#X}, expected: {:#X?}",
                    buf.len(),
                    self.pos,
                    other
                ),
            }
        }

        self.switch_source(next_file_ptr);
        // The engine has independent readers per `.lin`; reads stay
        // within the current source. Cross-source spill is not
        // engine-correct (verified via SC xbe RE: each `.lin` has
        // its own `MmAllocateContiguousMemoryEx`-backed data buffer
        // and there are two separate `compressed_file_reader`
        // instances). The runtime is responsible for calling
        // `switch_to_source` before invoking `load_linker` for a
        // package known to live in a different `.lin`.
        let cur_idx = self.current_source_idx;
        let source = &mut self.sources[cur_idx];
        let bytes_read = source.read(buf)?;
        self.pos += bytes_read as u64;
        self.source_consumed += bytes_read as u64;
        self.source_consumed_per_source[cur_idx] += bytes_read as u64;
        if let Some(top) = self.capture_stack.last_mut() {
            top.extend_from_slice(&buf[..bytes_read]);
        }
        if let Some(linker) = self.linker.last_mut() {
            linker.borrow_mut().set_position(self.pos);
        }

        if self.linker_header_depth == 0
            && self.trace_ops_consumed > 0
            && let Some(expected) = self.expected_source_per_op.as_ref()
            && let Some(&exp) = expected.get(self.trace_ops_consumed as usize - 1)
        {
            let drift = self.source_consumed as i64 - exp as i64;
            if drift != self.last_drift {
                eprintln!(
                    "drift change at read trace op #{}: ours={:#X}, engine={:#X}, diff={:+} (was {:+})",
                    self.trace_ops_consumed - 1,
                    self.source_consumed,
                    exp,
                    drift,
                    self.last_drift,
                );
                self.last_drift = drift;
            }
        }

        Ok(bytes_read)
    }
}

impl<R: Read> Seek for CheckedLinReader<R> {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        let span = span!(Level::TRACE, "seek");
        let _enter = span.enter();

        let mut next_file_ptr: u32 = 0;
        let res = match pos {
            std::io::SeekFrom::Start(pos) => {
                trace!("to= {:#X}, from= {:#X}", pos, self.pos);

                if self.linker_header_depth == 0 {
                    let mut ops = self.io_ops.borrow_mut();

                    match ops
                        .pop_front()
                        .expect("conducting an IO op but there are no more IO ops")
                    {
                        IoOp::Seek { to, from, file_ptr, .. } => {
                            next_file_ptr = file_ptr;
                            // Not checking `from` because there's some weird nuance with EOF
                            if from != self.pos || to != pos {
                                let remaining = ops.len();
                                let cur_file_ptr = self
                                    .file_ptr_to_index
                                    .iter()
                                    .find(|&(_, &v)| v == self.current_source_idx)
                                    .map(|(&k, _)| k)
                                    .unwrap_or(0);
                                panic!(
                                    "Attempted to seek from {:#X} to {:#X} (cur file_ptr=0x{:X}); should be seeking from {:#X} to {:#X} (file_ptr=0x{:X}). Linker stack: [{:#X?}]; remaining ops: {}",
                                    self.pos,
                                    pos,
                                    cur_file_ptr,
                                    from,
                                    to,
                                    file_ptr,
                                    self.linker
                                        .iter()
                                        .map(|linker| {
                                            let linker = linker.borrow();

                                            format!("{}: pos_saved={:#X} src_start={:#X}", linker.name, linker.reader_offset, linker.source_start)
                                        })
                                        .collect::<Vec<_>>()
                                        .join(", "),
                                    remaining,
                                );
                            }
                            self.trace_ops_consumed += 1;
                            if let Some(expected) = self.expected_source_per_op.as_ref()
                                && let Some(&exp) = expected.get(self.trace_ops_consumed as usize - 1)
                            {
                                let drift = self.source_consumed as i64 - exp as i64;
                                if drift != self.last_drift {
                                    eprintln!(
                                        "drift change at seek trace op #{}: ours={:#X}, engine={:#X}, diff={:+} (was {:+})",
                                        self.trace_ops_consumed - 1,
                                        self.source_consumed,
                                        exp,
                                        drift,
                                        self.last_drift,
                                    );
                                    self.last_drift = drift;
                                }
                            }
                        }
                        other => {
                            let bytes_until_next_seek = ops
                                .iter()
                                .take_while(|op| !matches!(op, IoOp::Seek { .. }))
                                .fold(0, |accum, op| {
                                    if let IoOp::Read { len, .. } = op {
                                        accum + *len
                                    } else {
                                        unreachable!("unexpected op");
                                    }
                                });
                            panic!(
                                "doing a seek from {:#X} to {:#X}. Bytes until next seek: {bytes_until_next_seek:#X}. Expected op: {other:#X?}. source_consumed_per_source: {:#X?}, current_source_idx: {}",
                                self.pos, pos, self.source_consumed_per_source, self.current_source_idx
                            )
                        }
                    }
                }

                self.pos = pos;
                Ok(pos)
            }
            std::io::SeekFrom::End(_) => todo!("end position seeking not implemented"),
            std::io::SeekFrom::Current(0) => Ok(self.pos),
            std::io::SeekFrom::Current(_) => todo!("current position seeking not implemented"),
        };
        self.switch_source(next_file_ptr);
        if let Some(linker) = self.linker.last_mut() {
            linker.borrow_mut().set_position(self.pos);
        }

        res
    }
}

pub trait LinRead: io::Read + io::Seek {
    fn set_reading_linker_header(&mut self, reading_linker_header: bool);
    fn cheat(&mut self, buf: &mut [u8]) -> io::Result<()>;
    /// Read `buf.len()` bytes from the underlying source as if a *different*
    /// FArchive (sharing the same FFile) had issued the read. The bytes are
    /// removed from the source cursor and one matching trace `Read` op is
    /// popped, but `self.pos` is left untouched — mirroring the engine's
    /// behavior when one FArchive's Serialize call advances the shared FFile
    /// past the linker's logical archive position. Used by
    /// `USound::Serialize`'s inline lipsynch load: SC opens a separate
    /// FArchive for the `.bin`, reads the whole body, then frees the archive
    /// before continuing the outer linker's serialize.
    fn read_aliased(&mut self, buf: &mut [u8]) -> io::Result<()>;
    fn push_linker(&mut self, linker: RcLinker);
    fn pop_linker(&mut self) -> RcLinker;
    /// Begin capturing bytes read into a new buffer. Subsequent `read`
    /// and `cheat` calls append to the *innermost* capture frame, so
    /// nested preloads correctly attribute their bytes to their own
    /// frame (not the outer one).
    fn push_capture(&mut self);
    /// End the innermost capture frame and return the bytes read while
    /// it was active.
    fn pop_capture(&mut self) -> Vec<u8>;
    /// Length of the innermost capture frame (0 if none active). Used
    /// with `splice_capture_tail` to substitute the bytes captured for
    /// a phantom field (e.g. UStruct::ScriptText) — SC's
    /// `UStruct::Serialize` reads ScriptText into a discarded local on
    /// load and writes a null on save, so the LIN bytes for that field
    /// are noise that breaks UExplorer when re-emitted verbatim.
    fn capture_len(&self) -> usize;
    /// Owned copy of the innermost capture frame's bytes from `from..`.
    /// Used by callers that want to extract a contiguous sub-region of
    /// the in-progress body (e.g. just the script-bytecode section)
    /// without ending the capture frame.
    fn capture_slice_from(&self, from: usize) -> Vec<u8>;
    /// Replace the trailing `remove` bytes of the innermost capture
    /// frame with `replacement`. No-op if no capture is active.
    fn splice_capture_tail(&mut self, remove: usize, replacement: &[u8]);
    /// Cumulative bytes consumed from the underlying source. Unaffected
    /// by virtual seeks. Used to map sequential decompressed-`.lin`
    /// offsets to the right export bodies for static recompilation.
    fn source_consumed(&self) -> u64;
    /// Cumulative bytes consumed per source. Same total as
    /// `source_consumed()` but broken down so the EOF safety net can
    /// pinpoint which `.lin` has leftover.
    fn source_consumed_per_source(&self) -> &[u64];
    /// Set the source-absolute start of the current package. All
    /// subsequent `seek(SeekFrom::Start(virtual))` calls translate to
    /// `source_start + virtual` when seeking the underlying source.
    /// Used by `load_linker` to set the package base before parsing
    /// its header (which seeks to package-relative name/import/export
    /// table offsets).
    fn set_source_start(&mut self, start: u64);
    /// Trace ops consumed so far. Returns 0 for `LinReader` (no trace).
    /// Useful for diagnostic prints when an assertion is about to fire.
    fn trace_ops_consumed(&self) -> u64 {
        0
    }
    /// Trace ops still queued (not yet consumed). Returns 0 for
    /// `LinReader` since it has no trace.
    fn trace_ops_remaining(&self) -> usize {
        0
    }
    /// Switch the active source to the given index. The runtime triggers
    /// this at known boundary points where the engine moves between
    /// `.lin` files (in particular, the `None.MyLevel` bootstrap entry
    /// for the typical SC layout where the level lives in the second
    /// `.lin`).
    fn switch_to_source(&mut self, _idx: usize) {}
}

impl<R> LinRead for LinReader<R>
where
    R: Read,
{
    fn set_reading_linker_header(&mut self, _reading_linker_header: bool) {}

    fn cheat(&mut self, buf: &mut [u8]) -> io::Result<()> {
        self.read_exact(buf)
    }

    fn read_aliased(&mut self, buf: &mut [u8]) -> io::Result<()> {
        // No trace: read straight from the source. We deliberately do NOT
        // advance `self.pos` so the surrounding archive's logical position
        // matches the engine's (whose lipsynch FArchive is independent of
        // the outer linker). Source consumption tracking advances naturally
        // through the read.
        let saved_pos = self.pos;
        self.read_exact(buf)?;
        self.pos = saved_pos;
        Ok(())
    }

    fn switch_to_source(&mut self, idx: usize) {
        if idx < self.sources.len() {
            self.current_source_idx = idx;
        }
    }

    fn push_linker(&mut self, linker: RcLinker) {
        if let Some(prev) = self.linker.last() {
            prev.borrow_mut().set_position(self.pos);
        }
        self.source_start_stack.push(self.source_start);
        self.source_start = linker.borrow().source_start;
        self.pos = linker.borrow().reader_offset;
        debug!(
            "push_linker {} (depth now {}): source_start={:#X}, pos={:#X}",
            linker.borrow().name,
            self.linker.len() + 1,
            self.source_start,
            self.pos
        );
        self.linker.push(linker);
    }

    fn pop_linker(&mut self) -> RcLinker {
        let linker = self.linker.pop().expect("no linker");
        linker.borrow_mut().set_position(self.pos);
        self.source_start = self.source_start_stack.pop().unwrap_or(0);
        if let Some(prev) = self.linker.last() {
            self.pos = prev.borrow().reader_offset;
        }
        debug!(
            "pop_linker {} (depth now {}): source_start={:#X}, pos={:#X}",
            linker.borrow().name,
            self.linker.len(),
            self.source_start,
            self.pos
        );
        linker
    }

    fn push_capture(&mut self) {
        self.capture_stack.push(Vec::new());
    }

    fn pop_capture(&mut self) -> Vec<u8> {
        self.capture_stack.pop().unwrap_or_default()
    }

    fn capture_len(&self) -> usize {
        self.capture_stack.last().map(|v| v.len()).unwrap_or(0)
    }

    fn capture_slice_from(&self, from: usize) -> Vec<u8> {
        self.capture_stack
            .last()
            .map(|v| v.get(from..).map(|s| s.to_vec()).unwrap_or_default())
            .unwrap_or_default()
    }

    fn splice_capture_tail(&mut self, remove: usize, replacement: &[u8]) {
        if let Some(top) = self.capture_stack.last_mut() {
            let new_len = top.len().saturating_sub(remove);
            top.truncate(new_len);
            top.extend_from_slice(replacement);
        }
    }

    fn source_consumed(&self) -> u64 {
        self.source_consumed
    }

    fn source_consumed_per_source(&self) -> &[u64] {
        &self.source_consumed_per_source
    }

    fn set_source_start(&mut self, start: u64) {
        self.source_start = start;
    }

    fn trace_ops_consumed(&self) -> u64 {
        0
    }
}

impl<R> LinRead for CheckedLinReader<R>
where
    R: Read,
{
    fn set_reading_linker_header(&mut self, reading_linker_header: bool) {
        if reading_linker_header {
            self.linker_header_depth = self
                .linker_header_depth
                .checked_add(1)
                .expect("linker_header_depth overflow");
        } else {
            self.linker_header_depth = self
                .linker_header_depth
                .checked_sub(1)
                .expect("linker_header_depth underflow: unbalanced set_reading_linker_header(false)");
        }
    }

    fn cheat(&mut self, buf: &mut [u8]) -> io::Result<()> {
        // Empty-buffer call mirrors the engine's `Ar.Serialize(buf, 0)`
        // (e.g. an empty `TArray<BYTE>` lazy mip body). The kernel still
        // records that as a `Read(len=0)` syscall, so we pop exactly one
        // op of that shape. Asserting `len == 0` keeps the call explicit:
        // the only way to reach this branch is the engine's matching
        // 0-byte serialize, and the trace must have that 0-byte op queued.
        if buf.is_empty() {
            let mut io_ops = self.io_ops.borrow_mut();
            match io_ops.pop_front().expect("no io op?") {
                IoOp::Read { len, .. } => {
                    assert_eq!(
                        len, 0,
                        "cheat(empty) expected Read(len=0) but got Read(len={len:#X})"
                    );
                }
                other => panic!("cheat(empty) expected Read(len=0) but got {other:#X?}"),
            }
            drop(io_ops);
            self.trace_ops_consumed += 1;
            return Ok(());
        }
        let mut remove_len = 0;
        let mut popped_count: u64 = 0;
        let mut io_ops = self.io_ops.borrow_mut();
        let mut last_file_ptr: u32 = 0;
        while remove_len < buf.len() {
            match io_ops.pop_front().expect("no io op?") {
                IoOp::Seek { from, to, file_ptr, .. } => {
                    let stack = self
                        .linker
                        .iter()
                        .map(|l| {
                            let l = l.borrow();
                            format!(
                                "{}: pos_saved={:#X} src_start={:#X}",
                                l.name, l.reader_offset, l.source_start
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    panic!(
                        "unexpected seek op while cheating reads: \
                         seek from={:#X} to={:#X} file_ptr={:#X}; \
                         buf.len={:#X} popped_count={} remove_len={:#X} \
                         cur_pos={:#X} linker_stack=[{}]",
                        from,
                        to,
                        file_ptr,
                        buf.len(),
                        popped_count,
                        remove_len,
                        self.pos,
                        stack
                    );
                }
                IoOp::Read { len, file_ptr, .. } => {
                    last_file_ptr = file_ptr;
                    remove_len += len as usize;
                    popped_count += 1;
                }
            }
        }
        assert_eq!(remove_len, buf.len());
        if remove_len > 0 {
            io_ops.push_front(IoOp::Read {
                len: buf.len() as u64,
                file_ptr: last_file_ptr,
                archive_open: -1,
                archive_pos: 0,
            });
        }
        drop(io_ops);
        if popped_count > 1 {
            self.trace_ops_consumed += popped_count - 1;
        }
        self.read_exact(buf)
    }

    fn read_aliased(&mut self, buf: &mut [u8]) -> io::Result<()> {
        // Pop the one matching trace Read op without going through the
        // assertion-rich `<Self as Read>::read` path: source advances by
        // buf.len() bytes but the linker's logical archive position stays
        // put. Mirrors the engine's behavior when one FArchive (the
        // lipsynch loader) does Ar.Serialize on a SECOND archive that
        // shares the same underlying FFile — the FFile's position
        // advances; the OUTER linker's logical position doesn't.
        if buf.is_empty() {
            return Ok(());
        }
        if self.linker_header_depth == 0 {
            let mut ops = self.io_ops.borrow_mut();
            match ops
                .pop_front()
                .expect("read_aliased: no io op queued")
            {
                IoOp::Read { len, file_ptr, .. } => {
                    assert_eq!(
                        buf.len() as u64,
                        len,
                        "read_aliased expected Read(len={:#X}), got Read(len={:#X})",
                        buf.len(),
                        len
                    );
                    self.trace_ops_consumed += 1;
                    drop(ops);
                    self.switch_source(file_ptr);
                }
                other => panic!(
                    "read_aliased: expected Read op but got {:#X?} at pos {:#X}",
                    other, self.pos
                ),
            }
        }
        let cur_idx = self.current_source_idx;
        let saved_pos = self.pos;
        // Read from the current source. We deliberately bypass `<Self as
        // Read>::read` to avoid the per-op pop+assert path (already done
        // above) and the self.pos advance.
        let n = self.sources[cur_idx].read(buf)?;
        if n != buf.len() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "read_aliased: source short read",
            ));
        }
        self.source_consumed += n as u64;
        self.source_consumed_per_source[cur_idx] += n as u64;
        if let Some(top) = self.capture_stack.last_mut() {
            top.extend_from_slice(&buf[..n]);
        }
        self.pos = saved_pos;
        if let Some(linker) = self.linker.last_mut() {
            linker.borrow_mut().set_position(self.pos);
        }
        Ok(())
    }

    fn push_linker(&mut self, linker: RcLinker) {
        if let Some(prev) = self.linker.last() {
            prev.borrow_mut().set_position(self.pos);
        }
        self.source_start_stack.push(self.source_start);
        self.source_start = linker.borrow().source_start;
        self.pos = linker.borrow().reader_offset;
        debug!(
            "push_linker {} (depth now {}): source_start={:#X}, pos={:#X}",
            linker.borrow().name,
            self.linker.len() + 1,
            self.source_start,
            self.pos
        );
        self.linker.push(linker);
    }

    fn pop_linker(&mut self) -> RcLinker {
        let linker = self.linker.pop().expect("no linker?");
        linker.borrow_mut().set_position(self.pos);
        self.source_start = self.source_start_stack.pop().unwrap_or(0);
        if let Some(prev) = self.linker.last() {
            self.pos = prev.borrow().reader_offset;
        }
        debug!(
            "pop_linker {} (depth now {}): source_start={:#X}, pos={:#X}",
            linker.borrow().name,
            self.linker.len(),
            self.source_start,
            self.pos
        );
        linker
    }

    fn push_capture(&mut self) {
        self.capture_stack.push(Vec::new());
    }

    fn pop_capture(&mut self) -> Vec<u8> {
        self.capture_stack.pop().unwrap_or_default()
    }

    fn capture_len(&self) -> usize {
        self.capture_stack.last().map(|v| v.len()).unwrap_or(0)
    }

    fn capture_slice_from(&self, from: usize) -> Vec<u8> {
        self.capture_stack
            .last()
            .map(|v| v.get(from..).map(|s| s.to_vec()).unwrap_or_default())
            .unwrap_or_default()
    }

    fn splice_capture_tail(&mut self, remove: usize, replacement: &[u8]) {
        if let Some(top) = self.capture_stack.last_mut() {
            let new_len = top.len().saturating_sub(remove);
            top.truncate(new_len);
            top.extend_from_slice(replacement);
        }
    }

    fn source_consumed(&self) -> u64 {
        self.source_consumed
    }

    fn source_consumed_per_source(&self) -> &[u64] {
        &self.source_consumed_per_source
    }

    fn set_source_start(&mut self, start: u64) {
        self.source_start = start;
    }

    fn trace_ops_consumed(&self) -> u64 {
        self.trace_ops_consumed
    }

    fn trace_ops_remaining(&self) -> usize {
        self.io_ops.borrow().len()
    }

    fn switch_to_source(&mut self, idx: usize) {
        if idx < self.sources.len() {
            self.current_source_idx = idx;
        }
    }
}
