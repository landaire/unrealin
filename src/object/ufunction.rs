use bitflags::bitflags;
use byteorder::ReadBytesExt;
use tracing::{Level, debug, span};

use crate::object::{DeserializeUnrealObject, builtins::Link, ustruct::Struct};

#[derive(Default, Debug)]
pub struct Function {
    pub parent_object: Struct,

    params_size: u16,
    inative: u16,
    num_params: u8,
    operator_precedence: u8,
    return_value_offset: u16,
    function_flags: FunctionFlags,
}

impl DeserializeUnrealObject for Function {
    fn deserialize<E, R>(
        &mut self,
        runtime: &mut crate::runtime::UnrealRuntime,
        linker: &std::rc::Rc<std::cell::RefCell<crate::de::Linker>>,
        reader: &mut R,
    ) -> std::io::Result<()>
    where
        E: byteorder::ByteOrder,
        R: crate::reader::LinRead,
    {
        let span = span!(Level::DEBUG, "deserialize_function");
        let _enter = span.enter();

        self.parent_object
            .deserialize::<E, _>(runtime, linker, reader)?;

        let version = linker.borrow().version();

        if version <= 63 {
            debug!("params_size");
            self.params_size = reader.read_u16::<E>()?;
        }

        debug!("inative");
        self.inative = reader.read_u16::<E>()?;

        if version <= 63 {
            debug!("num_params");
            self.num_params = reader.read_u8()?;
        }

        debug!("operator_precedence");
        self.operator_precedence = reader.read_u8()?;

        if version <= 63 {
            debug!("return_value_offset");
            self.return_value_offset = reader.read_u16::<E>()?;
        }

        debug!("function_flags");
        self.function_flags = FunctionFlags::from_bits(reader.read_u32::<E>()?)
            .expect("failed to parse function flags");

        if self.function_flags.contains(FunctionFlags::NET) {
            todo!("deserialize function_flags");
        }

        self.num_params = 0;
        self.params_size = 0;

        // if let Some(child) = &self.parent_object.children {
        //     for property in &self.parent_object.properties {
        //         self.params_size = property.offset() + property.len();
        //         if property.flags().contains(PropertyFlags::ReturnParam) {
        //             self.return_value_offset = property.offset();
        //         }
        //     }
        // }

        Ok(())
    }
}

bitflags! {
    /// Function flags.
    #[derive(Default, Debug, Copy, Clone)]
    pub struct FunctionFlags: u32 {
        /// Function is final (prebindable, non-overridable function).
        const FINAL = 0x00000001;
        /// Function has been defined (not just declared).
        const DEFINED = 0x00000002;
        /// Function is an iterator.
        const ITERATOR = 0x00000004;
        /// Function is a latent state function.
        const LATENT = 0x00000008;
        /// Unary operator is a prefix operator.
        const PRE_OPERATOR = 0x00000010;
        /// Function cannot be reentered.
        const SINGULAR = 0x00000020;
        /// Function is network-replicated.
        const NET = 0x00000040;
        /// Function should be sent reliably on the network.
        const NET_RELIABLE = 0x00000080;
        /// Function executed on the client side.
        const SIMULATED = 0x00000100;
        /// Executable from command line.
        const EXEC = 0x00000200;
        /// Native function.
        const NATIVE = 0x00000400;
        /// Event function.
        const EVENT = 0x00000800;
        /// Operator function.
        const OPERATOR = 0x00001000;
        /// Static function.
        const STATIC = 0x00002000;
        /// Don't export intrinsic function to C++.
        const NO_EXPORT = 0x00004000;
        /// Function doesn't modify this object.
        const CONST = 0x00008000;
        /// Return value is purely dependent on parameters; no state dependencies or internal state changes.
        const INVARIANT = 0x00010000;
        /// Function is accessible in all classes (if overridden, parameters much remain unchanged).
        const PUBLIC = 0x00020000;
        /// Function is accessible only in the class it is defined in (cannot be overriden, but function name may be reused in subclasses.  IOW: if overridden, parameters don't need to match, and Super.Func() cannot be accessed since it's private.)
        const PRIVATE = 0x00040000;
        /// Function is accessible only in the class it is defined in and subclasses (if overridden, parameters much remain unchanged).
        const PROTECTED = 0x00080000;
        /// Function is actually a delegate.
        const DELEGATE = 0x00100000;
        /// Function is executed on servers (set by replication code if passes check)
        const NET_SERVER = 0x00200000;
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::object::{UObjectKind, UnrealObject, test_common::test_object_is_a};

    use super::*;

    pub fn expected_uobjectkind() -> impl IntoIterator<Item = UObjectKind> {
        [UObjectKind::Function]
            .iter()
            .cloned()
            .chain(crate::object::ustruct::tests::expected_uobjectkind())
    }

    #[test]
    fn test_is_a() {
        let test_obj = Function::default();

        test_object_is_a(&test_obj as &dyn UnrealObject, expected_uobjectkind());
    }
}
