use std::{cell::RefCell, rc::Rc};

use crate::{
    object::{DeserializeUnrealObject, ustruct::Struct},
    reader::LinRead,
    runtime::UnrealRuntime,
};

#[derive(Default, Debug)]
pub struct State {
    pub parent_object: Struct,
}

impl DeserializeUnrealObject for State {
    fn deserialize<E, R>(
        &mut self,
        runtime: &mut UnrealRuntime,
        linker: &Rc<RefCell<crate::de::Linker>>,
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
pub(crate) mod tests {
    use crate::object::{UObjectKind, UnrealObject, test_common::test_object_is_a};

    use super::*;

    pub fn expected_uobjectkind() -> impl IntoIterator<Item = UObjectKind> {
        [UObjectKind::State]
            .iter()
            .cloned()
            .chain(crate::object::ustruct::tests::expected_uobjectkind())
    }

    #[test]
    fn test_is_a() {
        let test_obj = State::default();

        test_object_is_a(&test_obj as &dyn UnrealObject, expected_uobjectkind());
    }
}
