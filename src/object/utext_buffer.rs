use std::{cell::RefCell, io, rc::Rc};

use byteorder::ReadBytesExt;
use tracing::{Level, debug, span, trace};

use crate::{
    de::Linker,
    object::{DeserializeUnrealObject, uobject::Object},
    reader::{LinRead, UnrealReadExt},
    runtime::UnrealRuntime,
};

#[derive(Default, Debug)]
pub struct TextBuffer {
    pub parent_object: Object,

    pub position: u32,
    pub top: u32,
    pub text: String,
}

impl DeserializeUnrealObject for TextBuffer {
    fn deserialize<E, R>(
        &mut self,
        runtime: &mut UnrealRuntime,
        linker: &Rc<RefCell<Linker>>,
        reader: &mut R,
    ) -> io::Result<()>
    where
        E: byteorder::ByteOrder,
        R: LinRead,
    {
        let span = span!(Level::DEBUG, "deserialize_text_buffer");
        let _enter = span.enter();

        self.parent_object
            .deserialize::<E, _>(runtime, linker, reader)?;

        debug!("Reading position");
        self.position = reader.read_u32::<E>()?;

        debug!("Reading top");
        self.top = reader.read_u32::<E>()?;

        debug!("Reading text");
        self.text = reader.read_string()?;

        trace!("{:?}", self);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::object::{UObjectKind, UnrealObject, test_common::test_object_is_a};

    use super::*;

    pub fn expected_uobjectkind() -> impl IntoIterator<Item = UObjectKind> {
        [UObjectKind::TextBuffer]
            .iter()
            .cloned()
            .chain(crate::object::uobject::tests::expected_uobjectkind())
    }

    #[test]
    fn test_is_a() {
        let test_obj = TextBuffer::default();

        test_object_is_a(&test_obj as &dyn UnrealObject, expected_uobjectkind());
    }
}
