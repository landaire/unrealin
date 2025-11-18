use std::{cell::RefCell, io::SeekFrom, rc::Rc};

use byteorder::ReadBytesExt;
use tracing::{Level, debug, span, trace};

use crate::{
    de::Linker,
    object::RcUnrealObject,
    reader::{LinRead, UnrealReadExt},
    runtime::UnrealRuntime,
};

pub fn deserialize_expr<E, R>(
    runtime: &mut UnrealRuntime,
    linker: &Rc<RefCell<Linker>>,
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
        // Handle debug info
        if *bytes_read < script_size {
            // NOTE: These are purposefully not counted towards
            // the read data size!
            let before_pos = reader.stream_position()?;
            let mut debug_tokens = Vec::new();
            let version = if let Ok(ExprToken::DebugInfo) = ExprToken::try_from(reader.read_u8()?) {
                let version = reader.read_u32::<E>()?;
                debug_tokens = vec![
                    Expr::Token(ExprToken::DebugInfo),
                    // TODO: Endianness
                    Expr::Data(version.to_le_bytes().to_vec()),
                ];

                Some(version)
            } else {
                None
            };

            reader.seek(SeekFrom::Start(before_pos))?;

            if let Some(100) = version {
                trace!("Reading actual debug info");
                debug_tokens.append(&mut deserialize_expr::<E, _>(
                    runtime,
                    linker,
                    reader,
                    bytes_read,
                    script_size,
                )?);
            }

            result.append(&mut debug_tokens);
        }

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

            // The size of the object pointer is 4 bytes on 32-bit platforms.
            // So we increase by 4.
            *bytes_read += ((after - before) as usize).next_multiple_of(4);

            obj
        }};
    }

    match token {
        ExprToken::LocalVariable | ExprToken::InstanceVariable | ExprToken::DefaultVariable => {
            let obj = read_object!();

            result.push(Expr::Object(obj));
        }
        ExprToken::Return => {
            result.append(&mut deserialize_expr::<E, _>(
                runtime,
                linker,
                reader,
                bytes_read,
                script_size,
            )?);
        }
        ExprToken::Switch => todo!(),
        ExprToken::Jump => todo!(),
        ExprToken::JumpIfNot => todo!(),
        ExprToken::Assert => todo!(),
        ExprToken::Case => todo!(),
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
        ExprToken::LabelTable => todo!(),
        ExprToken::GotoLabel => todo!(),
        ExprToken::EatString => todo!(),
        ExprToken::Let => todo!(),
        ExprToken::DynArrayElement => todo!(),
        ExprToken::New => todo!(),
        ExprToken::ClassContext => todo!(),
        ExprToken::MetaCast => todo!(),
        ExprToken::LetBool => todo!(),
        ExprToken::LineNumber => todo!(),
        ExprToken::Skip => todo!(),
        ExprToken::Context => todo!(),
        ExprToken::ArrayElement => todo!(),
        ExprToken::VirtualFunction => todo!(),
        ExprToken::FinalFunction => todo!(),
        ExprToken::IntConst => todo!(),
        ExprToken::FloatConst => todo!(),
        ExprToken::StringConst => todo!(),
        ExprToken::ObjectConst => todo!(),
        ExprToken::NameConst => todo!(),
        ExprToken::RotationConst => todo!(),
        ExprToken::VectorConst => todo!(),
        ExprToken::ByteConst => todo!(),
        ExprToken::NativeParm => {
            let obj = read_object!();
            result.push(Expr::Object(obj));
        }
        ExprToken::IntConstByte => todo!(),
        ExprToken::DynamicCast => todo!(),
        ExprToken::Iterator => todo!(),
        ExprToken::StructCmpEq => todo!(),
        ExprToken::StructCmpNe => todo!(),
        ExprToken::UnicodeStringConst => todo!(),
        ExprToken::RangeConst => todo!(),
        ExprToken::StructMember => todo!(),
        ExprToken::DynArrayLength => todo!(),
        ExprToken::GlobalFunction => todo!(),
        ExprToken::PrimitiveCast => todo!(),
        ExprToken::DynArrayInsert => todo!(),
        ExprToken::DynArrayRemove => todo!(),
        ExprToken::DebugInfo => todo!(),
        ExprToken::DelegateFunction => todo!(),
        ExprToken::DelegateProperty => todo!(),
        ExprToken::LetDelegate => todo!(),
        ExprToken::PointerConst => todo!(),
        ExprToken::ExtendedNative => todo!(),
        ExprToken::FirstNative => todo!(),
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
