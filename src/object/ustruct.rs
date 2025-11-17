use std::{cell::RefCell, rc::Rc};

use byteorder::ReadBytesExt;
use tracing::{Level, debug, span};

use crate::{
    de::Linker,
    object::{
        DeserializeUnrealObject, RcUnrealObject, UObjectKind, builtins::Property, internal::script,
        ufield::Field, uobject::Object,
    },
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

        let mut script = Vec::new();
        let start_pos = reader.stream_position()?;
        let expected_end_pos = start_pos + self.script_size as u64;
        debug!(
            "deserializing script. start_pos= {start_pos:#X}, expected_end= {expected_end_pos:#X}, len= {:#X}",
            self.script_size
        );

        let mut bytes_read = 0;

        while bytes_read < self.script_size as usize {
            script.append(&mut script::deserialize_expr::<E, _>(
                runtime,
                linker,
                reader,
                &mut bytes_read,
                self.script_size as usize,
            )?);
        }

        assert_eq!(
            bytes_read, self.script_size as usize,
            "Did not read the expected amount of script data"
        );

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

            if !child_inner.is_a(UObjectKind::Property) {
                let parent_field = child_inner
                    .parent_of_kind(UObjectKind::Field)
                    .expect("could not get parent Field");

                child_ptr = parent_field
                    .as_any()
                    .downcast_ref::<Field>()
                    .expect("failed to cast parent field to Field")
                    .next();

                continue;
            }

            // TODO: Property work

            let parent_property = child_inner
                .parent_of_kind(UObjectKind::Property)
                .expect("failed to find child's parent Property");
            let child_as_property = parent_property
                .as_any()
                .downcast_ref::<Property>()
                .expect("failed to cast parent property to Property");

            child_ptr = child_as_property.parent_object.next();
        }

        // Handle properties with flags
        let mut child_ptr = self.children.clone();
        while let Some(child) = child_ptr {
            let span = span!(Level::DEBUG, "ustruct_property");
            let _enter = span.enter();

            let child_inner = child.borrow();
            if !child_inner.is_a(UObjectKind::Property) {
                break;
            }

            // TODO: Can remove from here
            let child_any = child_inner.as_any();
            let child_as_field = child_any
                .downcast_ref::<Field>()
                .expect("failed to cast child as Field");

            let next = child_as_field.next();

            if child_inner.is_a(UObjectKind::Property) {
                child_ptr = next;
                continue;
            }

            drop(child_inner);
            // TODO: to here

            let mut child_inner = child.borrow_mut();
            let child_any = child_inner.as_any_mut();
            let child_as_property = child_any
                .downcast_mut::<Property>()
                .expect("failed to cast child as Field");

            todo!("handle property");

            child_ptr = next;
        }

        Ok(())
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
