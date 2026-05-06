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
        let actual_len = string_len.abs() as usize;

        if is_unicode {
            // Unicode strings - read as wide chars (not implemented yet)
            panic!("Unicode strings not yet implemented");
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
    source: R,
    pos: u64,
    /// Bytes consumed from `source` so far. Unlike `pos` (virtual), this
    /// tracks the actual sequential byte stream position. Used by the
    /// serializer to map exports back to their byte ranges in the
    /// decompressed `.lin` for verbatim copy.
    source_consumed: u64,
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
        LinReader {
            source: reader,
            pos: 0,
            source_consumed: 0,
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

    pub fn set_source_start(&mut self, start: u64) {
        self.source_start = start;
    }
}

impl<R> Read for LinReader<R>
where
    R: Read,
{
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let bytes_read = self.source.read(buf)?;
        self.pos += bytes_read as u64;
        self.source_consumed += bytes_read as u64;
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
    io_ops: Rc<RefCell<VecDeque<IoOp>>>,
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
        CheckedLinReader {
            sources,
            file_ptr_to_index,
            file_ptr_order: VecDeque::from(file_ptr_order),
            current_source_idx: 0,
            pos: 0,
            source_consumed: 0,
            capture_stack: Vec::new(),
            source_start: 0,
            source_start_stack: Vec::new(),
            linker_header_depth: 0,
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
                IoOp::Read { len, file_ptr } => {
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
        let source = &mut self.sources[self.current_source_idx];
        let bytes_read = source.read(buf)?;
        self.pos += bytes_read as u64;
        self.source_consumed += bytes_read as u64;
        if let Some(top) = self.capture_stack.last_mut() {
            top.extend_from_slice(&buf[..bytes_read]);
        }
        if let Some(linker) = self.linker.last_mut() {
            linker.borrow_mut().set_position(self.pos);
        }

        Ok(bytes_read)
    }
}

impl<R> Seek for CheckedLinReader<R> {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        let span = span!(Level::TRACE, "seek");
        let _enter = span.enter();

        let mut next_file_ptr: u32 = 0;
        let mut do_source_seek_to: Option<u64> = None;
        let res = match pos {
            std::io::SeekFrom::Start(pos) => {
                trace!("to= {:#X}, from= {:#X}", pos, self.pos);

                if self.linker_header_depth == 0 {
                    let mut ops = self.io_ops.borrow_mut();

                    match ops
                        .pop_front()
                        .expect("conducting an IO op but there are no more IO ops")
                    {
                        IoOp::Seek { to, from, file_ptr } => {
                            next_file_ptr = file_ptr;
                            do_source_seek_to = Some(to);
                            // Not checking `from` because there's some weird nuance with EOF
                            if from != self.pos || to != pos {
                                let remaining = ops.len();
                                panic!(
                                    "Attempted to seek from {:#X} to {:#X}; should be seeking from {:#X} to {:#X}. Linker position: {:#X?}; remaining ops: {}",
                                    self.pos,
                                    pos,
                                    from,
                                    to,
                                    self.linker
                                        .iter()
                                        .map(|linker| {
                                            let linker = linker.borrow();

                                            format!("{}: {:#X}", linker.name, linker.reader_offset)
                                        })
                                        .collect::<Vec<_>>()
                                        .join(", "),
                                    remaining,
                                );
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
                                "doing a seek from {:#X} to {:#X}. Bytes until next seek: {bytes_until_next_seek:#X}. Expected op: {other:#X?}",
                                self.pos, pos
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
        let _ = do_source_seek_to;
        if let Some(linker) = self.linker.last_mut() {
            linker.borrow_mut().set_position(self.pos);
        }

        res
    }
}

pub trait LinRead: io::Read + io::Seek {
    fn set_reading_linker_header(&mut self, reading_linker_header: bool);
    fn cheat(&mut self, buf: &mut [u8]) -> io::Result<()>;
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
    /// Cumulative bytes consumed from the underlying source. Unaffected
    /// by virtual seeks. Used to map sequential decompressed-`.lin`
    /// offsets to the right export bodies for static recompilation.
    fn source_consumed(&self) -> u64;
    /// Set the source-absolute start of the current package. All
    /// subsequent `seek(SeekFrom::Start(virtual))` calls translate to
    /// `source_start + virtual` when seeking the underlying source.
    /// Used by `load_linker` to set the package base before parsing
    /// its header (which seeks to package-relative name/import/export
    /// table offsets).
    fn set_source_start(&mut self, start: u64);
}

impl<R> LinRead for LinReader<R>
where
    R: Read,
{
    fn set_reading_linker_header(&mut self, _reading_linker_header: bool) {}

    fn cheat(&mut self, buf: &mut [u8]) -> io::Result<()> {
        self.read_exact(buf)
    }

    fn push_linker(&mut self, linker: RcLinker) {
        if let Some(prev) = self.linker.last() {
            prev.borrow_mut().set_position(self.pos);
        }
        self.source_start_stack.push(self.source_start);
        self.source_start = linker.borrow().source_start;
        self.pos = linker.borrow().reader_offset;
        self.linker.push(linker);
    }

    fn pop_linker(&mut self) -> RcLinker {
        let linker = self.linker.pop().expect("no linker");
        linker.borrow_mut().set_position(self.pos);
        self.source_start = self.source_start_stack.pop().unwrap_or(0);
        if let Some(prev) = self.linker.last() {
            self.pos = prev.borrow().reader_offset;
        }
        linker
    }

    fn push_capture(&mut self) {
        self.capture_stack.push(Vec::new());
    }

    fn pop_capture(&mut self) -> Vec<u8> {
        self.capture_stack.pop().unwrap_or_default()
    }

    fn source_consumed(&self) -> u64 {
        self.source_consumed
    }

    fn set_source_start(&mut self, start: u64) {
        self.source_start = start;
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
        // Remove however many io ops are part of this read
        let mut remove_len = 0;

        let mut io_ops = self.io_ops.borrow_mut();
        let mut last_file_ptr: u32 = 0;
        while remove_len < buf.len() {
            match io_ops.pop_front().expect("no io op?") {
                IoOp::Seek { .. } => panic!("unexpected seek op while cheating reads"),
                IoOp::Read { len, file_ptr } => {
                    last_file_ptr = file_ptr;
                    remove_len += len as usize;
                }
            }
        }

        assert_eq!(remove_len, buf.len());

        // Insert a fake read of this exact size. 0-sized reads are short-circuited
        // by read_exact, so don't add this read if the data size is zero since the IO op
        // will never be popped.
        if remove_len > 0 {
            io_ops.push_front(IoOp::Read {
                len: buf.len() as u64,
                file_ptr: last_file_ptr,
            });
        }

        drop(io_ops);

        self.read_exact(buf)
    }

    fn push_linker(&mut self, linker: RcLinker) {
        if let Some(prev) = self.linker.last() {
            prev.borrow_mut().set_position(self.pos);
        }
        self.source_start_stack.push(self.source_start);
        self.source_start = linker.borrow().source_start;
        self.pos = linker.borrow().reader_offset;
        self.linker.push(linker);
    }

    fn pop_linker(&mut self) -> RcLinker {
        let linker = self.linker.pop().expect("no linker?");
        linker.borrow_mut().set_position(self.pos);
        self.source_start = self.source_start_stack.pop().unwrap_or(0);
        if let Some(prev) = self.linker.last() {
            self.pos = prev.borrow().reader_offset;
        }
        linker
    }

    fn push_capture(&mut self) {
        self.capture_stack.push(Vec::new());
    }

    fn pop_capture(&mut self) -> Vec<u8> {
        self.capture_stack.pop().unwrap_or_default()
    }

    fn source_consumed(&self) -> u64 {
        self.source_consumed
    }

    fn set_source_start(&mut self, start: u64) {
        self.source_start = start;
    }
}
