use std::{
    cell::RefCell,
    collections::{BTreeMap, VecDeque},
    io::{self, Read, Seek},
    rc::Rc,
};

use byteorder::{ByteOrder, ReadBytesExt};
use tracing::{Level, span, trace};

use crate::{
    common::IoOp,
    de::{ExportIndex, ImportIndex, Linker, RcLinker},
    object::{RcUnrealObject, UnrealObject},
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

    fn read_string(&mut self) -> io::Result<String> {
        let mut string_data = self.read_array()?;
        // Remove the null terminator
        let _ = string_data.pop();
        Ok(String::from_utf8(string_data).expect("string is not valid UTF-8"))
    }
}

impl<R: LinRead + Sized> UnrealReadExt for R {}

pub struct LinReader<R> {
    source: R,
    pos: u64,
    version: u16,
}

impl<R> LinReader<R> {
    pub fn new(reader: R) -> Self {
        LinReader {
            source: reader,
            pos: 0,
            version: 0,
        }
    }
}

impl<R> Read for LinReader<R>
where
    R: Read,
{
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let bytes_read = self.source.read(buf)?;
        self.pos += bytes_read as u64;

        Ok(bytes_read)
    }
}

impl<R> Seek for LinReader<R> {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        match pos {
            std::io::SeekFrom::Start(pos) => {
                self.pos = pos;
                Ok(pos)
            }
            std::io::SeekFrom::End(_) => todo!("end position seeking not implemented"),
            std::io::SeekFrom::Current(0) => Ok(self.pos),
            std::io::SeekFrom::Current(_) => todo!("current position seeking not implemented"),
        }
    }
}

pub struct CheckedLinReader<R> {
    source: R,
    pos: u64,
    version: u16,
    /// Package headers are not included in the raw IO ops
    reading_linker_header: bool,
    io_ops: Rc<RefCell<VecDeque<IoOp>>>,
}

impl<R> CheckedLinReader<R> {
    pub fn new(reader: R, io_ops: Rc<RefCell<VecDeque<IoOp>>>) -> Self {
        CheckedLinReader {
            source: reader,
            pos: 0,
            reading_linker_header: false,
            io_ops,
            version: 0,
        }
    }
}

impl<R> Read for CheckedLinReader<R>
where
    R: Read,
{
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if !self.reading_linker_header {
            let mut ops = self.io_ops.borrow_mut();

            match ops
                .pop_front()
                .expect("conducting an IO op but there are no more IO ops")
            {
                IoOp::Read { len } => {
                    assert_eq!(
                        buf.len() as u64,
                        len,
                        "Expected a read of {:#X} bytes, got read of {:#X} instead",
                        len,
                        buf.len()
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

        let bytes_read = self.source.read(buf)?;
        self.pos += bytes_read as u64;

        Ok(bytes_read)
    }
}

impl<R> Seek for CheckedLinReader<R> {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        let span = span!(Level::TRACE, "seek");
        let _enter = span.enter();

        match pos {
            std::io::SeekFrom::Start(pos) => {
                trace!("to= {:#X}, from= {:#X}", pos, self.pos);

                if !self.reading_linker_header {
                    let mut ops = self.io_ops.borrow_mut();

                    match ops
                        .pop_front()
                        .expect("conducting an IO op but there are no more IO ops")
                    {
                        IoOp::Seek { to, from } => {
                            if self.pos != from || to != pos {
                                panic!(
                                    "Attempted to seek from {:#X} to {:#X}; should be seeking from {:#X} to {:#X}",
                                    self.pos, pos, from, to
                                );
                            }
                        }
                        other => panic!(
                            "doing a seek from {:#X} to {:#X}. Expected op: {other:#X?}",
                            self.pos, pos
                        ),
                    }
                }

                self.pos = pos;
                Ok(pos)
            }
            std::io::SeekFrom::End(_) => todo!("end position seeking not implemented"),
            std::io::SeekFrom::Current(0) => Ok(self.pos),
            std::io::SeekFrom::Current(_) => todo!("current position seeking not implemented"),
        }
    }
}

pub trait LinRead: io::Read + io::Seek {
    fn set_reading_linker_header(&mut self, reading_linker_header: bool);
    fn cheat(&mut self, buf: &mut [u8]) -> io::Result<()>;
}

impl<R> LinRead for LinReader<R>
where
    R: Read,
{
    fn set_reading_linker_header(&mut self, _reading_linker_header: bool) {
        // Do nothing
    }

    fn cheat(&mut self, buf: &mut [u8]) -> io::Result<()> {
        // We have no IO ops to cheat
        self.read_exact(buf)
    }
}

impl<R> LinRead for CheckedLinReader<R>
where
    R: Read,
{
    fn set_reading_linker_header(&mut self, reading_linker_header: bool) {
        self.reading_linker_header = reading_linker_header;
    }

    fn cheat(&mut self, buf: &mut [u8]) -> io::Result<()> {
        // Remove however many io ops are part of this read
        let mut remove_len = 0;

        let mut io_ops = self.io_ops.borrow_mut();
        while remove_len < buf.len() {
            match io_ops.pop_front().expect("no io op?") {
                IoOp::Seek { to, from } => panic!("unexpected seek op while cheating reads"),
                IoOp::Read { len } => {
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
            });
        }

        drop(io_ops);

        self.read_exact(buf)
    }
}
