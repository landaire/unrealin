use std::{
    cell::RefCell,
    io::{Read, Seek},
    rc::Rc,
};

use byteorder::ReadBytesExt;
use tracing::{Level, debug, span, trace};

use crate::{
    de::Linker,
    object::{DeserializeUnrealObject, RcUnrealObject, UObjectKind, uobject::Object},
    reader::{LinRead, UnrealReadExt},
    runtime::UnrealRuntime,
};

#[derive(Default, Debug)]
pub struct Field {
    pub parent_object: Object,

    super_field: Option<RcUnrealObject>,
    next: Option<RcUnrealObject>,
}

impl Field {
    pub(crate) fn super_field(&self) -> Option<RcUnrealObject> {
        self.super_field.clone()
    }

    pub fn next(&self) -> Option<RcUnrealObject> {
        self.next.clone()
    }
}

impl DeserializeUnrealObject for Field {
    fn deserialize<E, R>(
        &mut self,
        runtime: &mut UnrealRuntime,
        linker: &Rc<RefCell<Linker>>,
        reader: &mut R,
    ) -> std::io::Result<()>
    where
        E: byteorder::ByteOrder,
        R: LinRead,
    {
        let span = span!(Level::DEBUG, "deserialize_field");
        let _enter = span.enter();

        self.parent_object
            .deserialize::<E, _>(runtime, linker, reader)?;

        trace!("deserializing super_field");
        self.super_field = reader.read_object::<E>(runtime, linker)?;

        trace!("deserializing next");
        self.next = reader.read_object::<E>(runtime, linker)?;

        debug!("{:?}", self);

        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::object::{UObjectKind, UnrealObject, test_common::test_object_is_a};

    use super::*;

    pub fn expected_uobjectkind() -> impl IntoIterator<Item = UObjectKind> {
        [UObjectKind::Field]
            .iter()
            .cloned()
            .chain(crate::object::uobject::tests::expected_uobjectkind())
    }

    #[test]
    fn test_is_a() {
        let test_obj = Field::default();

        test_object_is_a(&test_obj as &dyn UnrealObject, expected_uobjectkind());
    }
}
