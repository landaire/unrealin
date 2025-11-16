use std::{cell::RefCell, rc::Rc};

use byteorder::ReadBytesExt;
use tracing::{Level, debug, span};

use crate::{
    de::Linker,
    object::{DeserializeUnrealObject, RcUnrealObject, ufield::Field, uobject::Object},
    reader::{LinRead, UnrealReadExt},
    runtime::UnrealRuntime,
};

#[derive(Default, Debug)]
pub struct Struct {
    pub parent_object: Field,

    script_text: Option<RcUnrealObject>,
    pub children: Option<RcUnrealObject>,

    friendly_name: i32,

    flags: u32,
    line: u32,
    text_pos: u32,
    script_size: u32,
    script: Vec<u8>,
}

impl DeserializeUnrealObject for Struct {
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
        let span = span!(Level::DEBUG, "deserialize_struct");
        let _enter = span.enter();

        let licensee_version = linker.borrow().licensee_version();

        self.parent_object
            .deserialize::<E, _>(runtime, linker, reader)?;

        debug!("deserializing script_text");
        self.script_text = reader.read_object::<E>(runtime, linker)?;

        debug!("deserializing children");
        self.children = reader.read_object::<E>(runtime, linker)?;

        debug!("deserializing friendly_name");
        self.friendly_name = reader.read_packed_int()?;

        if licensee_version > 0x1A {
            self.flags = reader.read_u32::<E>()?;
        }

        debug!("deserializing line");
        self.line = reader.read_u32::<E>()?;

        debug!("deserializing text_pos");
        self.text_pos = reader.read_u32::<E>()?;

        debug!("deserializing script_size");
        self.script_size = reader.read_u32::<E>()?;

        debug!("deserializing script");
        // We don't need to actually parse this
        self.script = vec![0u8; self.script_size as usize];
        reader.cheat(self.script.as_mut_slice())?;

        // Deserialize properties
        //
        // First, ensure that the super field is fully loaded
        if let Some(super_field) = self.parent_object.super_field() {
            let (linker, export_index) = {
                let super_field = super_field.borrow();
                (
                    super_field.base_object().linker(),
                    super_field.base_object().export_index(),
                )
            };

            panic!("About to make sure that the parent object is fully loaded");

            runtime.load_object_by_export_index::<E, _>(
                export_index,
                &linker,
                crate::runtime::LoadKind::Load,
                reader,
            )?;
        }

        let mut child_ptr = self.children.clone();
        while let Some(child) = child_ptr {
            let span = span!(Level::DEBUG, "ustruct_property");
            let _enter = span.enter();

            let (linker, export_index) = {
                let super_field = child.borrow();
                (
                    super_field.base_object().linker(),
                    super_field.base_object().export_index(),
                )
            };

            runtime.load_object_by_export_index::<E, _>(
                export_index,
                &linker,
                crate::runtime::LoadKind::Load,
                reader,
            )?;

            let child_inner = child.borrow();
            let child_any = child_inner.as_any();
            let child_as_field = child_any
                .downcast_ref::<Field>()
                .expect("failed to cast child as Field");

            child_ptr = child_as_field.next();
        }

        todo!("struct")
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::object::{UObjectKind, UnrealObject, test_common::test_object_is_a};

    use super::*;

    pub fn expected_uobjectkind() -> impl IntoIterator<Item = UObjectKind> {
        [UObjectKind::Struct]
            .iter()
            .cloned()
            .chain(crate::object::ufield::tests::expected_uobjectkind())
    }

    #[test]
    fn test_is_a() {
        let test_obj = Struct::default();

        test_object_is_a(&test_obj as &dyn UnrealObject, expected_uobjectkind());
    }
}
