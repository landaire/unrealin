use std::io;

use crate::{
    de::RcLinker,
    object::{
        DeserializeUnrealObject, RcUnrealObject, UnrealObject,
        internal::{
            fdependency::FDependency, fname::FName, property::PropertyTag,
            serialize_item::collect_struct_properties_from_parts,
        },
        uobject::read_tag_value,
        ustate::State,
    },
    reader::{LinRead, UnrealReadExt},
    runtime::UnrealRuntime,
};
use byteorder::ReadBytesExt;
use tracing::{Level, debug, span, trace};

#[derive(Default, Debug)]
pub struct Class {
    pub parent_object: State,

    pub old_class_record_size: Option<u32>,
    pub class_flags: u32,
    pub class_guid: (u32, u32, u32, u32),
    pub dependencies: Vec<FDependency>,
    pub package_imports: Vec<FName>,
    pub class_within: Option<RcUnrealObject>,
    pub class_config_name: FName,
    pub hide_categories: Vec<FName>,
    pub default_tags: Vec<PropertyTag>,
}

impl DeserializeUnrealObject for Class {
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
        let span = span!(Level::DEBUG, "deserialize_class");
        let _enter = span.enter();

        debug!("parent_object");
        self.parent_object
            .deserialize::<E, _>(runtime, linker, reader)?;

        let version = linker.borrow().version();
        if version <= 61 {
            self.old_class_record_size = Some(reader.read_u32::<E>()?);
        }

        self.class_flags = reader.read_u32::<E>()?;
        self.class_guid = (
            reader.read_u32::<E>()?,
            reader.read_u32::<E>()?,
            reader.read_u32::<E>()?,
            reader.read_u32::<E>()?,
        );

        debug!("dependencies");
        self.dependencies = reader.read_serializable_array::<E, FDependency>(runtime, linker)?;

        debug!("package_imports");
        self.package_imports = reader.read_serializable_array::<E, FName>(runtime, linker)?;

        if version >= 62 {
            debug!("class_within");
            self.class_within = reader.read_object::<E>(runtime, linker)?;
            debug!("class_config_name");
            self.class_config_name
                .deserialize::<E, _>(runtime, linker, reader)?;
        }

        if version >= 99 {
            debug!("hide_categories");
            self.hide_categories = reader.read_serializable_array::<E, FName>(runtime, linker)?;
        }

        // Pre-collect this class's property chain (own children + super)
        // so the default_tags loop can dispatch SerializeItem on
        // StructProperty/ArrayProperty values. We hold `&mut self`, so we
        // must NOT acquire a `borrow()` on the same `RefCell` for `self`;
        // instead we pull `children` and `super_field` directly from the
        // already-mutably-borrowed fields. Each child / super is a
        // separate `RefCell` from the one holding `&mut self`, so
        // borrowing through the chain is safe.
        let own_children = self.parent_object.parent_object.children.clone();
        let super_field = self.parent_object.parent_object.parent_object.super_field();
        let properties = collect_struct_properties_from_parts(own_children, super_field);
        debug!(
            "Class::deserialize: own+super property chain len={} (own_present={}, super_present={})",
            properties.len(),
            self.parent_object.parent_object.children.is_some(),
            self.parent_object.parent_object.parent_object.super_field().is_some(),
        );

        debug!("default_tags");
        self.default_tags.clear();
        loop {
            let mut tag = PropertyTag::default();
            tag.deserialize::<E, _>(runtime, linker, reader)?;
            if tag.name.is_none(&linker.borrow()) {
                break;
            }
            trace!(
                "tag value: type={} size={:#X}",
                tag.property_type, tag.size
            );
            read_tag_value::<E, _>(&tag, &properties, runtime, linker, reader)?;
            self.default_tags.push(tag);
        }

        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::object::{UObjectKind, UnrealObject, test_common::test_object_is_a};

    use super::*;

    pub fn expected_uobjectkind() -> impl IntoIterator<Item = UObjectKind> {
        [UObjectKind::Class]
            .iter()
            .cloned()
            .chain(crate::object::ustate::tests::expected_uobjectkind())
    }

    #[test]
    fn test_is_a() {
        let test_obj = Class::default();

        test_object_is_a(&test_obj as &dyn UnrealObject, expected_uobjectkind());
    }
}
