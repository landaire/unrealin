use std::io;

use tracing::{Level, span};

use crate::{
    de::RcLinker,
    object::{DeserializeUnrealObject, ufield::Field},
    reader::{LinRead, UnrealReadExt},
    runtime::UnrealRuntime,
};

#[derive(Default, Debug)]
pub struct Enum {
    pub parent_object: Field,

    names: Vec<i32>,
}

impl DeserializeUnrealObject for Enum {
    fn deserialize<E, R>(
        &mut self,
        runtime: &mut UnrealRuntime,
        linker: &RcLinker,
        reader: &mut R,
    ) -> io::Result<()>
    where
        E: byteorder::ByteOrder,
        R: LinRead,
    {
        let span = span!(Level::DEBUG, "deserialize_enum");
        let _enter = span.enter();

        self.parent_object
            .deserialize::<E, _>(runtime, linker, reader)?;

        self.names = reader.read_packed_int_array()?;

        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::object::{UObjectKind, UnrealObject, test_common::test_object_is_a};

    use super::*;

    pub fn expected_uobjectkind() -> impl IntoIterator<Item = UObjectKind> {
        [UObjectKind::Enum]
            .iter()
            .cloned()
            .chain(crate::object::ufield::tests::expected_uobjectkind())
    }

    #[test]
    fn test_is_a() {
        let test_obj = Enum::default();

        test_object_is_a(&test_obj as &dyn UnrealObject, expected_uobjectkind());
    }
}
