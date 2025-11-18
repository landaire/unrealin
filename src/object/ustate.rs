use std::{cell::RefCell, rc::Rc};

use byteorder::ReadBytesExt;
use tracing::{Level, span, trace};

use crate::{
    object::{DeserializeUnrealObject, ustruct::Struct},
    reader::LinRead,
    runtime::UnrealRuntime,
};

#[derive(Default, Debug)]
pub struct State {
    pub parent_object: Struct,

    probe_mask: u64,
    ignore_mask: u64,
    label_table_offset: u16,
    state_flags: u32,
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
        let span = span!(Level::DEBUG, "deserialize_state");
        let _enter = span.enter();

        self.parent_object
            .deserialize::<E, _>(runtime, linker, reader)?;

        trace!("probe_mask");
        self.probe_mask = reader.read_u64::<E>()?;
        trace!("ignore_mask");
        self.ignore_mask = reader.read_u64::<E>()?;
        trace!("label_table_offset");
        self.label_table_offset = reader.read_u16::<E>()?;
        trace!("state_flags");
        self.state_flags = reader.read_u32::<E>()?;
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
