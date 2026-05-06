use std::{cell::RefCell, io::SeekFrom, rc::Rc};

use byteorder::ReadBytesExt;
use tracing::{Level, debug, span, trace};

use crate::{
    de::{Linker, RcLinker},
    object::RcUnrealObject,
    reader::{LinRead, UnrealReadExt},
    runtime::UnrealRuntime,
};

fn handle_optional_debug_info<E, R>(
    runtime: &mut UnrealRuntime,
    linker: &RcLinker,
    reader: &mut R,
    bytes_read: &mut usize,
    script_size: usize,
) -> std::io::Result<Vec<Expr>>
where
    E: byteorder::ByteOrder,
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
        out.push(Expr::Token(ExprToken::DebugInfo));
        out.push(Expr::Data(v.to_le_bytes().to_vec()));
        version = Some(v);
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
) -> std::io::Result<Vec<Expr>>
where
    E: byteorder::ByteOrder,
    R: LinRead,
{
    let span = span!(Level::DEBUG, "deserialize_expr");
    let _enter = span.enter();

    let mut result = Vec::new();
    let token_value = reader.read_u8()?;
    *bytes_read += 1;

    // These do not map directly to a token
    if token_value >= ExprToken::ExtendedNative as u8 {
        debug!("Token implies native");
        result.push(Expr::Native(token_value));

        // This byte is only there for ExtendedNative
        if token_value < ExprToken::FirstNative as u8 {
            trace!("Reading extra byte for ExtendedNative");

            result.push(Expr::Data(vec![reader.read_u8()?]));
            *bytes_read += 1;
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
            let before = reader.stream_position()?;
            let obj = reader.read_object::<E>(runtime, linker)?;
            let after = reader.stream_position()?;
            // In-memory script slot for an object pointer is 4 bytes regardless
            // of how many bytes the packed_int consumed in the file.
            let _ = after - before;
            *bytes_read += 4;
            obj
        }};
    }

    macro_rules! read_fname {
        () => {{
            let before = reader.stream_position()?;
            let idx = reader.read_packed_int()?;
            let after = reader.stream_position()?;
            let _ = after - before;
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
            let mut sub =
                deserialize_expr::<E, _>(runtime, linker, reader, bytes_read, script_size)?;
            assert!(!sub.is_empty());
            sub
        }};
    }

    match token {
        ExprToken::LocalVariable | ExprToken::InstanceVariable | ExprToken::DefaultVariable => {
            let obj = read_object!();
            result.push(Expr::Object(obj));
        }
        ExprToken::Return | ExprToken::EatString | ExprToken::DynArrayLength => {
            result.append(&mut sub_expr!());
        }
        ExprToken::Switch => {
            let _ = read_byte!();
            result.append(&mut sub_expr!());
        }
        ExprToken::Jump => {
            let _ = read_word!();
        }
        ExprToken::JumpIfNot => {
            let _ = read_word!();
            result.append(&mut sub_expr!());
        }
        ExprToken::Assert => {
            let _ = read_word!();
            result.append(&mut sub_expr!());
        }
        ExprToken::Case => {
            let w = read_word!();
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
                let _line = read_int!();
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
            let _ = read_word!();
            let _ = read_byte!();
            result.append(&mut sub_expr!());
        }
        ExprToken::MetaCast | ExprToken::DynamicCast => {
            let obj = read_object!();
            result.push(Expr::Object(obj));
            result.append(&mut sub_expr!());
        }
        ExprToken::LineNumber => {
            // UE2 source UStruct::SerializeExpr doesn't enumerate this in the
            // switch (falls through to default error). Some script tooling
            // emits it though; treat as no payload to avoid spurious panics.
        }
        ExprToken::Skip => {
            let _ = read_word!();
            result.append(&mut sub_expr!());
        }
        ExprToken::VirtualFunction | ExprToken::GlobalFunction => {
            let _ = read_fname!();
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
            let obj = read_object!();
            result.push(Expr::Object(obj));
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
            let _ = read_int!();
        }
        ExprToken::FloatConst => {
            let _ = read_float!();
        }
        ExprToken::StringConst => {
            // Read bytes until null terminator.
            loop {
                let b = read_byte!();
                if b == 0 {
                    break;
                }
            }
        }
        ExprToken::UnicodeStringConst => {
            loop {
                let w = read_word!();
                if w == 0 {
                    break;
                }
            }
        }
        ExprToken::ObjectConst => {
            let obj = read_object!();
            result.push(Expr::Object(obj));
        }
        ExprToken::NameConst => {
            let _ = read_fname!();
        }
        ExprToken::RotationConst => {
            let _ = read_int!();
            let _ = read_int!();
            let _ = read_int!();
        }
        ExprToken::VectorConst => {
            let _ = read_float!();
            let _ = read_float!();
            let _ = read_float!();
        }
        ExprToken::ByteConst | ExprToken::IntConstByte => {
            let _ = read_byte!();
        }
        ExprToken::NativeParm => {
            let obj = read_object!();
            result.push(Expr::Object(obj));
        }
        ExprToken::Iterator => {
            result.append(&mut sub_expr!());
            let _ = read_word!();
        }
        ExprToken::StructCmpEq | ExprToken::StructCmpNe => {
            let obj = read_object!();
            result.push(Expr::Object(obj));
            result.append(&mut sub_expr!());
            result.append(&mut sub_expr!());
        }
        ExprToken::StructMember => {
            let obj = read_object!();
            result.push(Expr::Object(obj));
            result.append(&mut sub_expr!());
        }
        ExprToken::PrimitiveCast => {
            let _kind = read_byte!();
            result.append(&mut sub_expr!());
        }
        ExprToken::DynArrayInsert | ExprToken::DynArrayRemove => {
            result.append(&mut sub_expr!());
            result.append(&mut sub_expr!());
            result.append(&mut sub_expr!());
        }
        ExprToken::DebugInfo => {
            let _version = read_int!();
            let _line = read_int!();
            let _char_pos = read_int!();
            let _opcode = read_byte!();
        }
        ExprToken::DelegateFunction => {
            let obj = read_object!();
            result.push(Expr::Object(obj));
            let _ = read_fname!();
        }
        ExprToken::DelegateProperty => {
            let _ = read_fname!();
        }
        ExprToken::RangeConst | ExprToken::PointerConst => {
            // Not enumerated in UT2004's UStruct::SerializeExpr (default
            // appErrorf). Treat as no-op for now and let the next divergence
            // surface naturally.
        }
        ExprToken::ExtendedNative | ExprToken::FirstNative => {
            // Handled by the "native" branch above the switch.
            unreachable!("native tokens are handled before the switch");
        }
    }

    Ok(result)
}

#[derive(Clone, Debug)]
pub enum Expr {
    Token(ExprToken),
    Native(u8),
    Sequence(Vec<Expr>),
    Data(Vec<u8>),
    Object(Option<RcUnrealObject>),
    Name(i32),
    /// DebugInfo is handled specially since its size
    /// doesn't seem to contribute to the overall code size values
    DebugInfo(Vec<Expr>),
}

/// Evaluatable expression item types.
#[derive(Copy, Clone, Debug)]
#[repr(u8)]
pub enum ExprToken {
    // Variable references.
    /// A local variable.
    LocalVariable = 0x00,
    /// An object variable.
    InstanceVariable = 0x01,
    /// Default variable for a concrete object.
    DefaultVariable = 0x02,

    // Tokens.
    /// Return from function.
    Return = 0x04,
    /// Switch.
    Switch = 0x05,
    /// Goto a local address in code.
    Jump = 0x06,
    /// Goto if not expression.
    JumpIfNot = 0x07,
    /// Stop executing state code.
    Stop = 0x08,
    /// Assertion.
    Assert = 0x09,
    /// Case.
    Case = 0x0A,
    /// No operation.
    Nothing = 0x0B,
    /// Table of labels.
    LabelTable = 0x0C,
    /// Goto a label.
    GotoLabel = 0x0D,
    /// Ignore a dynamic string.
    EatString = 0x0E,
    /// Assign an arbitrary size value to a variable.
    Let = 0x0F,
    /// Dynamic array element.!!
    DynArrayElement = 0x10,
    /// New object allocation.
    New = 0x11,
    /// Class default metaobject context.
    ClassContext = 0x12,
    /// Metaclass cast.
    MetaCast = 0x13,
    /// Let boolean variable.
    LetBool = 0x14,
    /// Set current source code line number in stack frame.
    LineNumber = 0x15,
    /// End of function call parameters.
    EndFunctionParms = 0x16,
    /// Self object.
    SelfObj = 0x17,
    /// Skippable expression.
    Skip = 0x18,
    /// Call a function through an object context.
    Context = 0x19,
    /// Array element.
    ArrayElement = 0x1A,
    /// A function call with parameters.
    VirtualFunction = 0x1B,
    /// A prebound function call with parameters.
    FinalFunction = 0x1C,
    /// Int constant.
    IntConst = 0x1D,
    /// Floating point constant.
    FloatConst = 0x1E,
    /// String constant.
    StringConst = 0x1F,
    /// An object constant.
    ObjectConst = 0x20,
    /// A name constant.
    NameConst = 0x21,
    /// A rotation constant.
    RotationConst = 0x22,
    /// A vector constant.
    VectorConst = 0x23,
    /// A byte constant.
    ByteConst = 0x24,
    /// Zero.
    IntZero = 0x25,
    /// One.
    IntOne = 0x26,
    /// Bool True.
    True = 0x27,
    /// Bool False.
    False = 0x28,
    /// Native function parameter offset.
    NativeParm = 0x29,
    /// NoObject.
    NoObject = 0x2A,
    /// Int constant that requires 1 byte.
    IntConstByte = 0x2C,
    /// A bool variable which requires a bitmask.
    BoolVariable = 0x2D,
    /// Safe dynamic class casting.
    DynamicCast = 0x2E,
    /// Begin an iterator operation.
    Iterator = 0x2F,
    /// Pop an iterator level.
    IteratorPop = 0x30,
    /// Go to next iteration.
    IteratorNext = 0x31,
    /// Struct binary compare-for-equal.
    StructCmpEq = 0x32,
    /// Struct binary compare-for-unequal.
    StructCmpNe = 0x33,
    /// Unicode string constant.
    UnicodeStringConst = 0x34,
    /// A range constant.
    RangeConst = 0x35,
    /// Struct member.
    StructMember = 0x36,
    /// A dynamic array length for setting/getting
    DynArrayLength = 0x37,
    /// Call non-state version of a function.
    GlobalFunction = 0x38,
    /// A casting operator for primitives which reads the type as the subsequent byte
    PrimitiveCast = 0x39,
    /// Inserts into a dynamic array
    DynArrayInsert = 0x40,
    /// Removes from a dynamic array
    DynArrayRemove = 0x41,
    /// DEBUGGER Debug information
    DebugInfo = 0x42,
    /// Call to a delegate function
    DelegateFunction = 0x43,
    /// Delegate expression
    DelegateProperty = 0x44,
    /// Assignment to a delegate
    LetDelegate = 0x45,
    /// Int constant.
    PointerConst = 0x46,
    /// Last byte in script code
    EndOfScript = 0x47,

    // Natives.
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
