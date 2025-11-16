use std::{cell::RefCell, rc::Rc};

use crate::{
    object::{DeserializeUnrealObject, ustruct::Struct},
    reader::LinRead,
    runtime::UnrealRuntime,
};

#[derive(Default, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct State {
    pub parent_object: Struct,
}

impl DeserializeUnrealObject for State {
    fn deserialize<E, R>(
        &mut self,
        runtime: &mut UnrealRuntime,
        linker: Rc<RefCell<crate::de::Linker>>,
        reader: &mut R,
    ) -> std::io::Result<()>
    where
        E: byteorder::ByteOrder,
        R: LinRead,
    {
        self.parent_object
            .deserialize::<E, _>(runtime, linker, reader)?;
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use crate::object::{UObjectKind, UnrealObject, test_common::test_object_is_a};

    use super::*;

    #[test]
    fn test_is_a() {
        let expected_kinds = [
            UObjectKind::Object,
            UObjectKind::Struct,
            UObjectKind::Field,
            UObjectKind::State,
        ];
        let test_obj = State::default();

        test_object_is_a(&test_obj as &dyn UnrealObject, expected_kinds.as_slice());
    }
}
