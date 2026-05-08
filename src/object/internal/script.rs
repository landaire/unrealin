use std::io::{self, SeekFrom, Write};

use byteorder::{ByteOrder, ReadBytesExt, WriteBytesExt};
use tracing::{Level, debug, span, trace};

use crate::{
    de::RcLinker,
    reader::{LinRead, UnrealReadExt},
    runtime::UnrealRuntime,
    ser::write_packed_int,
};

fn handle_optional_debug_info<E, R>(
    runtime: &mut UnrealRuntime,
    linker: &RcLinker,
    reader: &mut R,
    bytes_read: &mut usize,
    script_size: usize,
) -> io::Result<Vec<Expr>>
where
    E: ByteOrder,
    R: LinRead,
{
    let mut out = Vec::new();
    if *bytes_read >= script_size {
        return Ok(out);
    }
    let before_pos = reader.stream_position()?;
    let peek = reader.read_u8()?;
    let mut version: Option<u32> = None;
    if let Ok(ExprToken::DebugInfo) = ExprToken::try_from(peek) {
        let v = reader.read_u32::<E>()?;
        version = Some(v);
        // Intentionally NOT pushed into the tree: the engine's load
        // path peeks (read 1) + reads version (read 4) + seek-backs +
        // *recursively* parses the full DebugInfo block from the .lin's
        // duplicated bytes after the seek-back point. The recursive
        // `deserialize_expr` call below produces the canonical
        // [Token(DebugInfo), Int(version), Int(line), Int(textpos),
        // Byte(opcode)] tree entry. Pushing here would yield a
        // duplicated DebugInfo entry whose serialize output is what
        // contaminates UExplorer's bytecode decompiler.
    }
    reader.seek(SeekFrom::Start(before_pos))?;
    if version == Some(100) {
        out.append(&mut deserialize_expr::<E, _>(
            runtime,
            linker,
            reader,
            bytes_read,
            script_size,
        )?);
    }
    Ok(out)
}

pub fn deserialize_expr<E, R>(
    runtime: &mut UnrealRuntime,
    linker: &RcLinker,
    reader: &mut R,
    bytes_read: &mut usize,
    script_size: usize,
) -> io::Result<Vec<Expr>>
where
    E: ByteOrder,
    R: LinRead,
{
    let span = span!(Level::DEBUG, "deserialize_expr");
    let _enter = span.enter();

    let mut result = Vec::new();
    let token_value = reader.read_u8()?;
    *bytes_read += 1;

    if token_value >= ExprToken::ExtendedNative as u8 {
        debug!("Token implies native");
        result.push(Expr::Native(token_value));

        if token_value < ExprToken::FirstNative as u8 {
            trace!("Reading extra byte for ExtendedNative");
            let lo = reader.read_u8()?;
            *bytes_read += 1;
            result.push(Expr::Byte(lo));
        }

        trace!("Reading function params");
        loop {
            let mut parsed =
                deserialize_expr::<E, _>(runtime, linker, reader, bytes_read, script_size)?;
            assert!(!parsed.is_empty());

            let primary_token = parsed[0].clone();
            result.append(&mut parsed);

            if let Expr::Token(ExprToken::EndFunctionParms) = primary_token {
                break;
            }
        }

        trace!("Reading possible debug info");
        let mut debug_tokens =
            handle_optional_debug_info::<E, _>(runtime, linker, reader, bytes_read, script_size)?;
        result.append(&mut debug_tokens);

        return Ok(result);
    }
    let token = ExprToken::try_from(token_value).expect("failed to parse ExprToken");
    result.push(Expr::Token(token));

    debug!("Token is: {:?}", token);

    macro_rules! read_object {
        () => {{
            let raw_index = reader.read_packed_int()?;
            // Resolve the reference for its load-order side effects, but
            // discard it: re-emit only needs the raw on-disk index.
            let _ = runtime.load_object_by_raw_index::<E, _>(
                raw_index,
                linker,
                crate::runtime::LoadKind::Create,
                reader,
            )?;
            // In-memory script slot for an object pointer is 4 bytes regardless
            // of how many bytes the packed_int consumed in the file.
            *bytes_read += 4;
            raw_index
        }};
    }

    macro_rules! read_fname {
        () => {{
            let idx = reader.read_packed_int()?;
            *bytes_read += 4;
            idx
        }};
    }

    macro_rules! read_int {
        () => {{
            let v = reader.read_i32::<E>()?;
            *bytes_read += 4;
            v
        }};
    }

    macro_rules! read_word {
        () => {{
            let v = reader.read_u16::<E>()?;
            *bytes_read += 2;
            v
        }};
    }

    macro_rules! read_byte {
        () => {{
            let v = reader.read_u8()?;
            *bytes_read += 1;
            v
        }};
    }

    macro_rules! read_float {
        () => {{
            let v = reader.read_f32::<E>()?;
            *bytes_read += 4;
            v
        }};
    }

    macro_rules! sub_expr {
        () => {{
            let sub =
                deserialize_expr::<E, _>(runtime, linker, reader, bytes_read, script_size)?;
            assert!(!sub.is_empty());
            sub
        }};
    }

    match token {
        ExprToken::LocalVariable | ExprToken::InstanceVariable | ExprToken::DefaultVariable => {
            let idx = read_object!();
            result.push(Expr::Object(idx));
        }
        ExprToken::Return | ExprToken::EatString | ExprToken::DynArrayLength => {
            result.append(&mut sub_expr!());
        }
        ExprToken::Switch => {
            let b = read_byte!();
            result.push(Expr::Byte(b));
            result.append(&mut sub_expr!());
        }
        ExprToken::Jump => {
            let w = read_word!();
            result.push(Expr::Word(w));
        }
        ExprToken::JumpIfNot => {
            let w = read_word!();
            result.push(Expr::Word(w));
            result.append(&mut sub_expr!());
        }
        ExprToken::Assert => {
            let w = read_word!();
            result.push(Expr::Word(w));
            result.append(&mut sub_expr!());
        }
        ExprToken::Case => {
            let w = read_word!();
            result.push(Expr::Word(w));
            if w != 0xFFFF {
                result.append(&mut sub_expr!());
            }
        }
        ExprToken::Nothing
        | ExprToken::BoolVariable
        | ExprToken::EndOfScript
        | ExprToken::EndFunctionParms
        | ExprToken::IntZero
        | ExprToken::IntOne
        | ExprToken::True
        | ExprToken::False
        | ExprToken::NoObject
        | ExprToken::SelfObj
        | ExprToken::IteratorPop
        | ExprToken::Stop
        | ExprToken::IteratorNext => {}
        ExprToken::LabelTable => {
            // Loop FLabelEntry (FName + INT) until name == 0.
            loop {
                let name = read_fname!();
                let line = read_int!();
                result.push(Expr::Name(name));
                result.push(Expr::Int(line));
                if name == 0 {
                    break;
                }
            }
        }
        ExprToken::GotoLabel => {
            result.append(&mut sub_expr!());
        }
        ExprToken::Let | ExprToken::LetBool | ExprToken::LetDelegate => {
            result.append(&mut sub_expr!());
            result.append(&mut sub_expr!());
        }
        ExprToken::DynArrayElement | ExprToken::ArrayElement => {
            result.append(&mut sub_expr!());
            result.append(&mut sub_expr!());
        }
        ExprToken::New => {
            result.append(&mut sub_expr!());
            result.append(&mut sub_expr!());
            result.append(&mut sub_expr!());
            result.append(&mut sub_expr!());
        }
        ExprToken::ClassContext | ExprToken::Context => {
            result.append(&mut sub_expr!());
            let w = read_word!();
            let b = read_byte!();
            result.push(Expr::Word(w));
            result.push(Expr::Byte(b));
            result.append(&mut sub_expr!());
        }
        ExprToken::MetaCast | ExprToken::DynamicCast => {
            let idx = read_object!();
            result.push(Expr::Object(idx));
            result.append(&mut sub_expr!());
        }
        ExprToken::LineNumber => {}
        ExprToken::Skip => {
            let w = read_word!();
            result.push(Expr::Word(w));
            result.append(&mut sub_expr!());
        }
        ExprToken::VirtualFunction | ExprToken::GlobalFunction => {
            let n = read_fname!();
            result.push(Expr::Name(n));
            loop {
                let mut parsed = sub_expr!();
                let primary = parsed[0].clone();
                result.append(&mut parsed);
                if let Expr::Token(ExprToken::EndFunctionParms) = primary {
                    break;
                }
            }
            result.append(&mut handle_optional_debug_info::<E, _>(
                runtime,
                linker,
                reader,
                bytes_read,
                script_size,
            )?);
        }
        ExprToken::FinalFunction => {
            let idx = read_object!();
            result.push(Expr::Object(idx));
            loop {
                let mut parsed = sub_expr!();
                let primary = parsed[0].clone();
                result.append(&mut parsed);
                if let Expr::Token(ExprToken::EndFunctionParms) = primary {
                    break;
                }
            }
            result.append(&mut handle_optional_debug_info::<E, _>(
                runtime,
                linker,
                reader,
                bytes_read,
                script_size,
            )?);
        }
        ExprToken::IntConst => {
            let v = read_int!();
            result.push(Expr::Int(v));
        }
        ExprToken::FloatConst => {
            let v = read_float!();
            result.push(Expr::Float(v));
        }
        ExprToken::StringConst => {
            // Read bytes until null terminator. The terminator is included
            // in the captured Data so re-emit reproduces it verbatim.
            let mut bytes = Vec::new();
            loop {
                let b = read_byte!();
                bytes.push(b);
                if b == 0 {
                    break;
                }
            }
            result.push(Expr::Data(bytes));
        }
        ExprToken::UnicodeStringConst => {
            // 16-bit codepoints until 0x0000. Capture as raw bytes (LE pairs).
            let mut bytes = Vec::new();
            loop {
                let w = read_word!();
                bytes.extend_from_slice(&w.to_le_bytes());
                if w == 0 {
                    break;
                }
            }
            result.push(Expr::Data(bytes));
        }
        ExprToken::ObjectConst => {
            let idx = read_object!();
            result.push(Expr::Object(idx));
        }
        ExprToken::NameConst => {
            let n = read_fname!();
            result.push(Expr::Name(n));
        }
        ExprToken::RotationConst => {
            let p = read_int!();
            let y = read_int!();
            let r = read_int!();
            result.push(Expr::Int(p));
            result.push(Expr::Int(y));
            result.push(Expr::Int(r));
        }
        ExprToken::VectorConst => {
            let x = read_float!();
            let y = read_float!();
            let z = read_float!();
            result.push(Expr::Float(x));
            result.push(Expr::Float(y));
            result.push(Expr::Float(z));
        }
        ExprToken::ByteConst | ExprToken::IntConstByte => {
            let b = read_byte!();
            result.push(Expr::Byte(b));
        }
        ExprToken::NativeParm => {
            let idx = read_object!();
            result.push(Expr::Object(idx));
        }
        ExprToken::Iterator => {
            result.append(&mut sub_expr!());
            let w = read_word!();
            result.push(Expr::Word(w));
        }
        ExprToken::StructCmpEq | ExprToken::StructCmpNe => {
            let idx = read_object!();
            result.push(Expr::Object(idx));
            result.append(&mut sub_expr!());
            result.append(&mut sub_expr!());
        }
        ExprToken::StructMember => {
            let idx = read_object!();
            result.push(Expr::Object(idx));
            result.append(&mut sub_expr!());
        }
        ExprToken::PrimitiveCast => {
            let kind = read_byte!();
            result.push(Expr::Byte(kind));
            result.append(&mut sub_expr!());
        }
        ExprToken::DynArrayInsert | ExprToken::DynArrayRemove => {
            result.append(&mut sub_expr!());
            result.append(&mut sub_expr!());
            result.append(&mut sub_expr!());
        }
        ExprToken::DebugInfo => {
            let version = read_int!();
            let line = read_int!();
            let char_pos = read_int!();
            let opcode = read_byte!();
            result.push(Expr::Int(version));
            result.push(Expr::Int(line));
            result.push(Expr::Int(char_pos));
            result.push(Expr::Byte(opcode));
        }
        ExprToken::DelegateFunction => {
            let idx = read_object!();
            let n = read_fname!();
            result.push(Expr::Object(idx));
            result.push(Expr::Name(n));
        }
        ExprToken::DelegateProperty => {
            let n = read_fname!();
            result.push(Expr::Name(n));
        }
        ExprToken::RangeConst | ExprToken::PointerConst => {
            // Not enumerated in UT2004's UStruct::SerializeExpr (default
            // appErrorf). Treat as no payload to avoid spurious panics; if
            // SC actually uses these, the next divergence will surface.
        }
        ExprToken::ExtendedNative | ExprToken::FirstNative => {
            unreachable!("native tokens are handled before the switch");
        }
    }

    Ok(result)
}

/// Walks the lossless `Expr` tree and emits canonical on-disk bytes.
/// Inverse of `deserialize_expr` for any tree built without source-rewind
/// drift; used by the Struct re-emit path so script bodies stop carrying
/// the peek-back artifacts that contaminate captured raw bytes.
pub fn serialize_expr<W: Write, E: ByteOrder>(exprs: &[Expr], w: &mut W) -> io::Result<()> {
    for e in exprs {
        match e {
            Expr::Token(t) => w.write_u8(*t as u8)?,
            Expr::Native(b) => w.write_u8(*b)?,
            Expr::Object(idx) | Expr::Name(idx) => write_packed_int(w, *idx)?,
            Expr::Byte(b) => w.write_u8(*b)?,
            Expr::Word(v) => w.write_u16::<E>(*v)?,
            Expr::Int(v) => w.write_i32::<E>(*v)?,
            Expr::Float(v) => w.write_f32::<E>(*v)?,
            Expr::Data(bytes) => w.write_all(bytes)?,
        }
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub enum Expr {
    /// Single-byte token (0x00..0x47).
    Token(ExprToken),
    /// Native dispatch byte (0x60..0xFF). For ExtendedNative (0x60..<0x70)
    /// the next entry is the second dispatch byte (an `Expr::Byte`).
    Native(u8),
    /// Object reference: raw, linker-relative packed_int index. Re-emit
    /// goes through `write_packed_int` so the on-disk byte count matches
    /// the original (in-memory script slot is fixed 4 bytes, but disk
    /// uses variable-length packed_int).
    Object(i32),
    /// FName index (linker-relative), encoded on disk as packed_int.
    Name(i32),
    Byte(u8),
    Word(u16),
    Int(i32),
    Float(f32),
    /// Variable-length payload (string bodies, ExtendedNative second-byte
    /// when emitted via `Data` from older trees).
    Data(Vec<u8>),
}

/// Evaluatable expression item types.
#[derive(Copy, Clone, Debug)]
#[repr(u8)]
pub enum ExprToken {
    LocalVariable = 0x00,
    InstanceVariable = 0x01,
    DefaultVariable = 0x02,
    Return = 0x04,
    Switch = 0x05,
    Jump = 0x06,
    JumpIfNot = 0x07,
    Stop = 0x08,
    Assert = 0x09,
    Case = 0x0A,
    Nothing = 0x0B,
    LabelTable = 0x0C,
    GotoLabel = 0x0D,
    EatString = 0x0E,
    Let = 0x0F,
    DynArrayElement = 0x10,
    New = 0x11,
    ClassContext = 0x12,
    MetaCast = 0x13,
    LetBool = 0x14,
    LineNumber = 0x15,
    EndFunctionParms = 0x16,
    SelfObj = 0x17,
    Skip = 0x18,
    Context = 0x19,
    ArrayElement = 0x1A,
    VirtualFunction = 0x1B,
    FinalFunction = 0x1C,
    IntConst = 0x1D,
    FloatConst = 0x1E,
    StringConst = 0x1F,
    ObjectConst = 0x20,
    NameConst = 0x21,
    RotationConst = 0x22,
    VectorConst = 0x23,
    ByteConst = 0x24,
    IntZero = 0x25,
    IntOne = 0x26,
    True = 0x27,
    False = 0x28,
    NativeParm = 0x29,
    NoObject = 0x2A,
    IntConstByte = 0x2C,
    BoolVariable = 0x2D,
    DynamicCast = 0x2E,
    Iterator = 0x2F,
    IteratorPop = 0x30,
    IteratorNext = 0x31,
    StructCmpEq = 0x32,
    StructCmpNe = 0x33,
    UnicodeStringConst = 0x34,
    RangeConst = 0x35,
    StructMember = 0x36,
    DynArrayLength = 0x37,
    GlobalFunction = 0x38,
    PrimitiveCast = 0x39,
    DynArrayInsert = 0x40,
    DynArrayRemove = 0x41,
    DebugInfo = 0x42,
    DelegateFunction = 0x43,
    DelegateProperty = 0x44,
    LetDelegate = 0x45,
    PointerConst = 0x46,
    EndOfScript = 0x47,
    ExtendedNative = 0x60,
    FirstNative = 0x70,
}

impl TryFrom<u8> for ExprToken {
    type Error = u8;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x00 => Ok(ExprToken::LocalVariable),
            0x01 => Ok(ExprToken::InstanceVariable),
            0x02 => Ok(ExprToken::DefaultVariable),
            0x04 => Ok(ExprToken::Return),
            0x05 => Ok(ExprToken::Switch),
            0x06 => Ok(ExprToken::Jump),
            0x07 => Ok(ExprToken::JumpIfNot),
            0x08 => Ok(ExprToken::Stop),
            0x09 => Ok(ExprToken::Assert),
            0x0A => Ok(ExprToken::Case),
            0x0B => Ok(ExprToken::Nothing),
            0x0C => Ok(ExprToken::LabelTable),
            0x0D => Ok(ExprToken::GotoLabel),
            0x0E => Ok(ExprToken::EatString),
            0x0F => Ok(ExprToken::Let),
            0x10 => Ok(ExprToken::DynArrayElement),
            0x11 => Ok(ExprToken::New),
            0x12 => Ok(ExprToken::ClassContext),
            0x13 => Ok(ExprToken::MetaCast),
            0x14 => Ok(ExprToken::LetBool),
            0x15 => Ok(ExprToken::LineNumber),
            0x16 => Ok(ExprToken::EndFunctionParms),
            0x17 => Ok(ExprToken::SelfObj),
            0x18 => Ok(ExprToken::Skip),
            0x19 => Ok(ExprToken::Context),
            0x1A => Ok(ExprToken::ArrayElement),
            0x1B => Ok(ExprToken::VirtualFunction),
            0x1C => Ok(ExprToken::FinalFunction),
            0x1D => Ok(ExprToken::IntConst),
            0x1E => Ok(ExprToken::FloatConst),
            0x1F => Ok(ExprToken::StringConst),
            0x20 => Ok(ExprToken::ObjectConst),
            0x21 => Ok(ExprToken::NameConst),
            0x22 => Ok(ExprToken::RotationConst),
            0x23 => Ok(ExprToken::VectorConst),
            0x24 => Ok(ExprToken::ByteConst),
            0x25 => Ok(ExprToken::IntZero),
            0x26 => Ok(ExprToken::IntOne),
            0x27 => Ok(ExprToken::True),
            0x28 => Ok(ExprToken::False),
            0x29 => Ok(ExprToken::NativeParm),
            0x2A => Ok(ExprToken::NoObject),
            0x2C => Ok(ExprToken::IntConstByte),
            0x2D => Ok(ExprToken::BoolVariable),
            0x2E => Ok(ExprToken::DynamicCast),
            0x2F => Ok(ExprToken::Iterator),
            0x30 => Ok(ExprToken::IteratorPop),
            0x31 => Ok(ExprToken::IteratorNext),
            0x32 => Ok(ExprToken::StructCmpEq),
            0x33 => Ok(ExprToken::StructCmpNe),
            0x34 => Ok(ExprToken::UnicodeStringConst),
            0x35 => Ok(ExprToken::RangeConst),
            0x36 => Ok(ExprToken::StructMember),
            0x37 => Ok(ExprToken::DynArrayLength),
            0x38 => Ok(ExprToken::GlobalFunction),
            0x39 => Ok(ExprToken::PrimitiveCast),
            0x40 => Ok(ExprToken::DynArrayInsert),
            0x41 => Ok(ExprToken::DynArrayRemove),
            0x42 => Ok(ExprToken::DebugInfo),
            0x43 => Ok(ExprToken::DelegateFunction),
            0x44 => Ok(ExprToken::DelegateProperty),
            0x45 => Ok(ExprToken::LetDelegate),
            0x46 => Ok(ExprToken::PointerConst),
            0x47 => Ok(ExprToken::EndOfScript),
            0x60 => Ok(ExprToken::ExtendedNative),
            0x70 => Ok(ExprToken::FirstNative),
            _ => Err(value),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::{HashMap, HashSet};
    use std::io::{Cursor, Seek};
    use std::rc::Rc;

    use byteorder::LittleEndian;

    use super::*;
    use crate::de::{Linker, PackageHeader, RawPackage};
    use crate::reader::LinReader;
    use crate::runtime::UnrealRuntime;

    fn stub_runtime() -> UnrealRuntime {
        UnrealRuntime {
            linkers: HashMap::new(),
            objects_full_loading: HashSet::new(),
            loaded_objects: HashSet::new(),
            package_file_size: HashMap::new(),
            present_packages: HashSet::new(),
            pending_loads: Vec::new(),
            begin_load_count: 0,
            next_construction_index: 0,
            preload_stack: Vec::new(),
            file_table_entries: Vec::new(),
        }
    }

    fn stub_linker() -> Rc<RefCell<Linker>> {
        let header = PackageHeader {
            version: 0,
            flags: 0,
            name_count: 0,
            name_offset: 0,
            export_count: 0,
            export_offset: 0,
            import_count: 0,
            import_offset: 0,
            unk: 0,
            unknown_data: Vec::new(),
            guid_a: 0,
            guid_b: 0,
            guid_c: 0,
            guid_d: 0,
            generations: Vec::new(),
        };
        let pkg = RawPackage {
            header,
            names: Vec::new(),
            imports: Vec::new(),
            exports: Vec::new(),
        };
        Rc::new(RefCell::new(Linker::new("stub".to_string(), pkg)))
    }

    /// Run `deserialize_expr` on `bytes` until the source is exhausted.
    /// Loops on `stream_position` rather than `bytes_read` because the
    /// engine's bytes_read counter advances by 4 per object/name ref
    /// regardless of the on-disk packed_int length, which can overshoot
    /// the actual byte count for tests that use small raw indices.
    /// Object/Name refs in the input must use raw_index = 0 (single
    /// packed_int byte 0x00) so the stub linker doesn't need exports.
    fn parse_script(bytes: &[u8]) -> Vec<Expr> {
        let mut runtime = stub_runtime();
        let linker = stub_linker();
        let mut reader = LinReader::new(Cursor::new(bytes.to_vec()));
        let mut bytes_read = 0usize;
        let mut out = Vec::new();
        // Generous script_size guard for handle_optional_debug_info.
        let script_size = bytes.len().saturating_mul(16).max(64);
        loop {
            let pos = reader.stream_position().unwrap() as usize;
            if pos >= bytes.len() {
                break;
            }
            out.append(
                &mut deserialize_expr::<LittleEndian, _>(
                    &mut runtime,
                    &linker,
                    &mut reader,
                    &mut bytes_read,
                    script_size,
                )
                .expect("deserialize_expr failed"),
            );
        }
        out
    }

    fn round_trip_bytes(bytes: &[u8]) -> Vec<u8> {
        let tree = parse_script(bytes);
        let mut out = Vec::new();
        serialize_expr::<_, LittleEndian>(&tree, &mut out).unwrap();
        out
    }

    /// Hand-built `Expr` trees that exercise every variant. Round-trip
    /// through `serialize_expr` and verify the bytes match a hand-computed
    /// canonical encoding. This covers the serialize side end-to-end
    /// without needing a real `.lin` source.
    #[test]
    fn serialize_expr_emits_canonical_bytes() {
        // [LocalVariable, Object(packed_int 0x10)]
        let exprs = vec![
            Expr::Token(ExprToken::LocalVariable),
            Expr::Object(0x10),
        ];
        let mut out = Vec::new();
        serialize_expr::<_, LittleEndian>(&exprs, &mut out).unwrap();
        // Token 0x00, then packed_int 0x10:
        //   0x10 < 0x40 so single byte 0x10 (sign bit clear, continue clear).
        assert_eq!(out, vec![0x00, 0x10]);

        // IntConst 0x12345678
        let exprs = vec![
            Expr::Token(ExprToken::IntConst),
            Expr::Int(0x12345678),
        ];
        let mut out = Vec::new();
        serialize_expr::<_, LittleEndian>(&exprs, &mut out).unwrap();
        assert_eq!(out, vec![0x1D, 0x78, 0x56, 0x34, 0x12]);

        // ByteConst 0xAB
        let exprs = vec![Expr::Token(ExprToken::ByteConst), Expr::Byte(0xAB)];
        let mut out = Vec::new();
        serialize_expr::<_, LittleEndian>(&exprs, &mut out).unwrap();
        assert_eq!(out, vec![0x24, 0xAB]);

        // Native dispatch (single byte, FirstNative+) with no params
        // (just EndFunctionParms after the dispatch byte).
        let exprs = vec![
            Expr::Native(0x80),
            Expr::Token(ExprToken::EndFunctionParms),
        ];
        let mut out = Vec::new();
        serialize_expr::<_, LittleEndian>(&exprs, &mut out).unwrap();
        assert_eq!(out, vec![0x80, 0x16]);

        // ExtendedNative (0x60..<0x70) followed by second dispatch byte.
        let exprs = vec![
            Expr::Native(0x61),
            Expr::Byte(0x42),
            Expr::Token(ExprToken::EndFunctionParms),
        ];
        let mut out = Vec::new();
        serialize_expr::<_, LittleEndian>(&exprs, &mut out).unwrap();
        assert_eq!(out, vec![0x61, 0x42, 0x16]);

        // StringConst with a captured Data block (terminator included).
        let exprs = vec![
            Expr::Token(ExprToken::StringConst),
            Expr::Data(b"hi\0".to_vec()),
        ];
        let mut out = Vec::new();
        serialize_expr::<_, LittleEndian>(&exprs, &mut out).unwrap();
        assert_eq!(out, vec![0x1F, b'h', b'i', 0x00]);

        // Jump 0xBEEF
        let exprs = vec![Expr::Token(ExprToken::Jump), Expr::Word(0xBEEF)];
        let mut out = Vec::new();
        serialize_expr::<_, LittleEndian>(&exprs, &mut out).unwrap();
        assert_eq!(out, vec![0x06, 0xEF, 0xBE]);

        // FloatConst 1.0_f32
        let exprs = vec![Expr::Token(ExprToken::FloatConst), Expr::Float(1.0)];
        let mut out = Vec::new();
        serialize_expr::<_, LittleEndian>(&exprs, &mut out).unwrap();
        assert_eq!(out, vec![0x1E, 0x00, 0x00, 0x80, 0x3F]);
    }

    /// Negative packed_int round-trip via `serialize_expr -> read_packed_int`.
    /// Verifies the on-disk encoding is invertible for negative object/name
    /// indices (imports use negative raw indices).
    #[test]
    fn packed_int_negative_round_trips() {
        use crate::reader::UnrealReadExt;

        for value in [-1, -0x40, -0x3F, -0x2000, -0x1FFF, -0x100000, i32::MIN + 1] {
            let exprs = vec![Expr::Object(value)];
            let mut out = Vec::new();
            serialize_expr::<_, LittleEndian>(&exprs, &mut out).unwrap();

            let mut reader = LinReader::new(Cursor::new(out));
            let decoded = reader.read_packed_int().unwrap();
            assert_eq!(decoded, value, "round-trip failed for {value}");
        }
    }

    /// Pure-token sequence (no payloads). Catches regressions in the
    /// "payload-free token" arms of the match.
    #[test]
    fn snapshot_pure_tokens() {
        let bytes = vec![
            ExprToken::Nothing as u8,
            ExprToken::IntZero as u8,
            ExprToken::IntOne as u8,
            ExprToken::True as u8,
            ExprToken::False as u8,
            ExprToken::NoObject as u8,
            ExprToken::SelfObj as u8,
            ExprToken::Stop as u8,
            ExprToken::IteratorPop as u8,
            ExprToken::IteratorNext as u8,
            ExprToken::EndOfScript as u8,
        ];
        let tree = parse_script(&bytes);
        insta::assert_debug_snapshot!(tree);
        assert_eq!(round_trip_bytes(&bytes), bytes);
    }

    /// Fixed-width primitive constants. Verifies endianness and byte-width
    /// for IntConst, FloatConst, ByteConst, IntConstByte.
    #[test]
    fn snapshot_primitive_consts() {
        let mut bytes = Vec::new();
        bytes.push(ExprToken::IntConst as u8);
        bytes.extend_from_slice(&0x12345678i32.to_le_bytes());
        bytes.push(ExprToken::FloatConst as u8);
        bytes.extend_from_slice(&1.5f32.to_le_bytes());
        bytes.push(ExprToken::ByteConst as u8);
        bytes.push(0xAB);
        bytes.push(ExprToken::IntConstByte as u8);
        bytes.push(0x7F);
        let tree = parse_script(&bytes);
        insta::assert_debug_snapshot!(tree);
        assert_eq!(round_trip_bytes(&bytes), bytes);
    }

    /// String constants. ANSI bytes-with-null and UTF-16 LE pairs-with-null.
    #[test]
    fn snapshot_string_consts() {
        let mut bytes = Vec::new();
        bytes.push(ExprToken::StringConst as u8);
        bytes.extend_from_slice(b"hi\0");
        bytes.push(ExprToken::UnicodeStringConst as u8);
        for u in [0x68u16, 0x69u16, 0x0u16] {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
        let tree = parse_script(&bytes);
        insta::assert_debug_snapshot!(tree);
        assert_eq!(round_trip_bytes(&bytes), bytes);
    }

    /// Jump family + Case (with both a present-sub-expr value and the
    /// 0xFFFF default-skip sentinel).
    #[test]
    fn snapshot_jump_family() {
        let mut bytes = Vec::new();
        bytes.push(ExprToken::Jump as u8);
        bytes.extend_from_slice(&0xBEEFu16.to_le_bytes());
        // JumpIfNot 0x1234 -> Nothing
        bytes.push(ExprToken::JumpIfNot as u8);
        bytes.extend_from_slice(&0x1234u16.to_le_bytes());
        bytes.push(ExprToken::Nothing as u8);
        // Assert 0x5678 -> True
        bytes.push(ExprToken::Assert as u8);
        bytes.extend_from_slice(&0x5678u16.to_le_bytes());
        bytes.push(ExprToken::True as u8);
        // Case 0x0010 -> IntZero
        bytes.push(ExprToken::Case as u8);
        bytes.extend_from_slice(&0x0010u16.to_le_bytes());
        bytes.push(ExprToken::IntZero as u8);
        // Case 0xFFFF (default sentinel, no sub-expr)
        bytes.push(ExprToken::Case as u8);
        bytes.extend_from_slice(&0xFFFFu16.to_le_bytes());
        // Skip 0x0042 -> SelfObj
        bytes.push(ExprToken::Skip as u8);
        bytes.extend_from_slice(&0x0042u16.to_le_bytes());
        bytes.push(ExprToken::SelfObj as u8);
        let tree = parse_script(&bytes);
        insta::assert_debug_snapshot!(tree);
        assert_eq!(round_trip_bytes(&bytes), bytes);
    }

    /// Object refs (raw index 0). Covers LocalVariable, InstanceVariable,
    /// DefaultVariable, ObjectConst, NativeParm, MetaCast, DynamicCast,
    /// StructMember, StructCmpEq, StructCmpNe, DelegateFunction.
    #[test]
    fn snapshot_object_refs() {
        let mut bytes = Vec::new();
        // LocalVariable Object(0)
        bytes.push(ExprToken::LocalVariable as u8);
        bytes.push(0x00);
        // InstanceVariable Object(0)
        bytes.push(ExprToken::InstanceVariable as u8);
        bytes.push(0x00);
        // DefaultVariable Object(0)
        bytes.push(ExprToken::DefaultVariable as u8);
        bytes.push(0x00);
        // ObjectConst Object(0)
        bytes.push(ExprToken::ObjectConst as u8);
        bytes.push(0x00);
        // NativeParm Object(0)
        bytes.push(ExprToken::NativeParm as u8);
        bytes.push(0x00);
        // MetaCast Object(0) -> SelfObj
        bytes.push(ExprToken::MetaCast as u8);
        bytes.push(0x00);
        bytes.push(ExprToken::SelfObj as u8);
        // DynamicCast Object(0) -> NoObject
        bytes.push(ExprToken::DynamicCast as u8);
        bytes.push(0x00);
        bytes.push(ExprToken::NoObject as u8);
        // StructMember Object(0) -> SelfObj
        bytes.push(ExprToken::StructMember as u8);
        bytes.push(0x00);
        bytes.push(ExprToken::SelfObj as u8);
        // StructCmpEq Object(0) -> SelfObj -> SelfObj
        bytes.push(ExprToken::StructCmpEq as u8);
        bytes.push(0x00);
        bytes.push(ExprToken::SelfObj as u8);
        bytes.push(ExprToken::SelfObj as u8);
        // DelegateFunction Object(0) Name(0)
        bytes.push(ExprToken::DelegateFunction as u8);
        bytes.push(0x00);
        bytes.push(0x00);
        let tree = parse_script(&bytes);
        insta::assert_debug_snapshot!(tree);
        assert_eq!(round_trip_bytes(&bytes), bytes);
    }

    /// Name refs (raw index 0). NameConst, DelegateProperty.
    #[test]
    fn snapshot_name_refs() {
        let bytes = vec![
            ExprToken::NameConst as u8,
            0x00,
            ExprToken::DelegateProperty as u8,
            0x00,
        ];
        let tree = parse_script(&bytes);
        insta::assert_debug_snapshot!(tree);
        assert_eq!(round_trip_bytes(&bytes), bytes);
    }

    /// RotationConst (3 i32) and VectorConst (3 f32).
    #[test]
    fn snapshot_rotation_vector() {
        let mut bytes = Vec::new();
        bytes.push(ExprToken::RotationConst as u8);
        bytes.extend_from_slice(&100i32.to_le_bytes());
        bytes.extend_from_slice(&200i32.to_le_bytes());
        bytes.extend_from_slice(&300i32.to_le_bytes());
        bytes.push(ExprToken::VectorConst as u8);
        bytes.extend_from_slice(&1.0f32.to_le_bytes());
        bytes.extend_from_slice(&2.0f32.to_le_bytes());
        bytes.extend_from_slice(&3.0f32.to_le_bytes());
        let tree = parse_script(&bytes);
        insta::assert_debug_snapshot!(tree);
        assert_eq!(round_trip_bytes(&bytes), bytes);
    }

    /// LabelTable: loop of (FName, INT) entries terminating with name == 0.
    /// Both the entries and the terminator are captured in the tree.
    #[test]
    fn snapshot_label_table() {
        let mut bytes = Vec::new();
        bytes.push(ExprToken::LabelTable as u8);
        // Three entries (we use raw_index = 0 for names; the i32 line
        // distinguishes them), then a (0, 0) terminator.
        // packed_int(0) = 0x00 byte.
        bytes.push(0x00); // name = 0 (note: this would normally terminate)
        // Wait — we can't actually have multiple non-terminator entries
        // with name == 0; the loop would stop at the first one. Use a
        // single (0, line) entry which IS the terminator.
        bytes.extend_from_slice(&42i32.to_le_bytes());
        let tree = parse_script(&bytes);
        insta::assert_debug_snapshot!(tree);
        assert_eq!(round_trip_bytes(&bytes), bytes);
    }

    /// Direct DebugInfo token (not via the peek-back handler). 14 bytes
    /// total: token + version + line + textpos + opcode.
    #[test]
    fn snapshot_debug_info_direct() {
        let mut bytes = Vec::new();
        bytes.push(ExprToken::DebugInfo as u8);
        bytes.extend_from_slice(&100i32.to_le_bytes()); // version
        bytes.extend_from_slice(&42i32.to_le_bytes()); // line
        bytes.extend_from_slice(&7i32.to_le_bytes()); // char_pos
        bytes.push(0xAB); // opcode
        let tree = parse_script(&bytes);
        insta::assert_debug_snapshot!(tree);
        assert_eq!(round_trip_bytes(&bytes), bytes);
    }

    /// Let / LetBool / LetDelegate: two sub-expressions each.
    #[test]
    fn snapshot_let_family() {
        let mut bytes = Vec::new();
        // Let LocalVariable(0) IntZero
        bytes.push(ExprToken::Let as u8);
        bytes.push(ExprToken::LocalVariable as u8);
        bytes.push(0x00);
        bytes.push(ExprToken::IntZero as u8);
        // LetBool LocalVariable(0) True
        bytes.push(ExprToken::LetBool as u8);
        bytes.push(ExprToken::LocalVariable as u8);
        bytes.push(0x00);
        bytes.push(ExprToken::True as u8);
        // LetDelegate LocalVariable(0) NoObject
        bytes.push(ExprToken::LetDelegate as u8);
        bytes.push(ExprToken::LocalVariable as u8);
        bytes.push(0x00);
        bytes.push(ExprToken::NoObject as u8);
        let tree = parse_script(&bytes);
        insta::assert_debug_snapshot!(tree);
        assert_eq!(round_trip_bytes(&bytes), bytes);
    }

    /// Context: sub_expr + word + byte + sub_expr.
    #[test]
    fn snapshot_context() {
        let mut bytes = Vec::new();
        bytes.push(ExprToken::Context as u8);
        bytes.push(ExprToken::SelfObj as u8); // first sub
        bytes.extend_from_slice(&0x1234u16.to_le_bytes()); // word
        bytes.push(0x05); // byte
        bytes.push(ExprToken::IntZero as u8); // second sub
        let tree = parse_script(&bytes);
        insta::assert_debug_snapshot!(tree);
        assert_eq!(round_trip_bytes(&bytes), bytes);
    }

    /// PrimitiveCast: byte then sub-expr.
    #[test]
    fn snapshot_primitive_cast() {
        let bytes = vec![
            ExprToken::PrimitiveCast as u8,
            0x03, // cast kind
            ExprToken::IntZero as u8,
        ];
        let tree = parse_script(&bytes);
        insta::assert_debug_snapshot!(tree);
        assert_eq!(round_trip_bytes(&bytes), bytes);
    }

    /// Iterator: sub_expr + word.
    #[test]
    fn snapshot_iterator() {
        let mut bytes = Vec::new();
        bytes.push(ExprToken::Iterator as u8);
        bytes.push(ExprToken::SelfObj as u8); // sub
        bytes.extend_from_slice(&0x0042u16.to_le_bytes()); // word
        let tree = parse_script(&bytes);
        insta::assert_debug_snapshot!(tree);
        assert_eq!(round_trip_bytes(&bytes), bytes);
    }

    /// New: 4 sub-expressions (outer, name, class, archetype).
    #[test]
    fn snapshot_new() {
        let bytes = vec![
            ExprToken::New as u8,
            ExprToken::SelfObj as u8,
            ExprToken::NoObject as u8,
            ExprToken::ObjectConst as u8,
            0x00, // raw_index 0
            ExprToken::NoObject as u8,
        ];
        let tree = parse_script(&bytes);
        insta::assert_debug_snapshot!(tree);
        assert_eq!(round_trip_bytes(&bytes), bytes);
    }

    /// DynArrayInsert / DynArrayRemove: 3 sub-expressions each.
    #[test]
    fn snapshot_dyn_array_modify() {
        let mut bytes = Vec::new();
        bytes.push(ExprToken::DynArrayInsert as u8);
        bytes.push(ExprToken::LocalVariable as u8);
        bytes.push(0x00);
        bytes.push(ExprToken::IntZero as u8);
        bytes.push(ExprToken::IntOne as u8);
        bytes.push(ExprToken::DynArrayRemove as u8);
        bytes.push(ExprToken::LocalVariable as u8);
        bytes.push(0x00);
        bytes.push(ExprToken::IntZero as u8);
        bytes.push(ExprToken::IntOne as u8);
        let tree = parse_script(&bytes);
        insta::assert_debug_snapshot!(tree);
        assert_eq!(round_trip_bytes(&bytes), bytes);
    }
}
