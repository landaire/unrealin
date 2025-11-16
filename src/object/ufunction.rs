use byteorder::ReadBytesExt;
use tracing::{Level, debug, span};

use crate::object::{DeserializeUnrealObject, ustruct::Struct};

#[derive(Default, Debug)]
pub struct Function {
    pub parent_object: Struct,

    params_size: u16,
    inative: u16,
    num_params: u8,
    operator_precedence: u8,
    return_value_offset: u16,
    function_flags: u32,
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
        self.function_flags = reader.read_u32::<E>()?;

        if self.function_flags != 0 {
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
