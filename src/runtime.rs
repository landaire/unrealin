use std::{
    cell::RefCell,
    collections::HashMap,
    io::{self, SeekFrom},
    rc::Rc,
};

use byteorder::ByteOrder;

use crate::object::DeserializeUnrealObject;
use crate::{
    de::{ExportIndex, ImportIndex, Linker, read_package},
    object::builtins::*,
    object::{ObjectFlags, UObjectKind, UnrealObject},
    reader::LinRead,
};

type RcLinker = Rc<RefCell<Linker>>;

pub struct UnrealRuntime {
    pub linkers: HashMap<String, Rc<RefCell<Linker>>>,
}

impl UnrealRuntime {
    fn load_linker<E, R>(&mut self, expected_name: String, reader: &mut R) -> io::Result<()>
    where
        R: LinRead,
        E: ByteOrder,
    {
        reader.set_reading_linker_header(true);
        let package = read_package::<E, _>(reader)?;
        reader.set_reading_linker_header(false);

        self.linkers.insert(
            expected_name.clone(),
            Rc::new(RefCell::new(Linker::new(expected_name, package))),
        );

        Ok(())
    }

    fn linker(&self, name: &str) -> Option<RcLinker> {
        self.linkers.get(name).map(Rc::clone)
    }

    fn find_object(&self, name: &str) -> Option<Rc<RefCell<dyn UnrealObject>>> {
        self.linkers.values().find_map(|linker| {
            linker
                .borrow()
                .objects
                .values()
                .find(|obj| obj.borrow().name() == name)
                .map(Rc::clone)
        })
    }

    fn linker_by_export_name_mut(&mut self, name: &str) -> Option<RcLinker> {
        let key = self.linkers.iter().find_map(|(name, linker)| {
            linker
                .borrow()
                .find_export_by_name(name)
                .map(|_| name.clone())
        });

        key.and_then(|k| self.linkers.get(&k).map(Rc::clone))
    }

    pub fn load_object_by_raw_index<E, R>(
        &mut self,
        raw_index: i32,
        linker: Rc<RefCell<Linker>>,
        reader: &mut R,
    ) -> io::Result<Option<Rc<RefCell<dyn UnrealObject>>>>
    where
        R: LinRead,
        E: ByteOrder,
    {
        if raw_index > 0 {
            self.load_object_by_export_index::<E, _>(
                ExportIndex::from_raw(raw_index),
                linker,
                reader,
            )
            .map(Some)
        } else if raw_index < 0 {
            let import_index = ImportIndex::from_raw(raw_index);

            // Grab this import's linker
            let linker_inner = linker.borrow();
            let import = linker_inner
                .find_import_by_index(import_index)
                .expect("failed to find import");
            let import_full_name = import.full_name(&linker_inner);

            self.load_object_by_full_name::<E, _>(import_full_name.as_str(), reader)
                .map(Some)
        } else {
            Ok(None)
        }
    }

    /// Loads and deserializes an object and its depencies by the export index.
    pub fn load_object_by_export_index<E, R>(
        &mut self,
        export_index: ExportIndex,
        linker: Rc<RefCell<Linker>>,
        reader: &mut R,
    ) -> io::Result<Rc<RefCell<dyn UnrealObject>>>
    where
        R: LinRead,
        E: ByteOrder,
    {
        let linker_inner = linker.borrow();
        let export = linker_inner
            .find_export_by_index(export_index)
            .expect("could not find export");
        let export_offset = export.serial_offset();
        let export_size = export.serial_size();

        println!("{:#X?}", export);

        let object_kind = UObjectKind::try_from(export.class_name(&linker_inner))
            .expect("could not find object kind");

        let constructed_object = object_kind.construct();
        let mut object = constructed_object.borrow_mut();
        object.set_flags(
            ObjectFlags::from_bits(export.object_flags).expect("failed to construct ObjectFlags"),
        );
        object.set_name(export.object_name(&linker_inner).to_owned());

        // If this is a struct, load the dependencies
        if object.is_a(UObjectKind::Struct) {
            let parent_index = export.super_index;

            if parent_index != 0 {
                // Load dependent types
                drop(linker_inner);

                self.load_object_by_raw_index::<E, _>(parent_index, Rc::clone(&linker), reader)?;
            }
        }

        drop(object);

        let saved_pos = reader.stream_position()?;
        reader.seek(SeekFrom::Start(export_offset))?;
        self.deserialize_object::<E, _>(
            Rc::clone(&constructed_object),
            Rc::clone(&linker),
            reader,
        )?;

        let current_pos = reader.stream_position()?;
        let read_size = (current_pos - export_offset) as usize;
        assert_eq!(
            read_size, export_size,
            "Data read for export does not match expected. Read {read_size:#X} bytes, expected {export_size:#X}",
        );

        reader.seek(SeekFrom::Start(saved_pos))?;

        linker
            .borrow_mut()
            .objects
            .insert(export_index, Rc::clone(&constructed_object));

        Ok(constructed_object)
    }

    pub fn load_object_by_full_name<E, R>(
        &mut self,
        full_name: &str,
        reader: &mut R,
    ) -> io::Result<Rc<RefCell<dyn UnrealObject>>>
    where
        R: LinRead,
        E: ByteOrder,
    {
        let mut parts = full_name.split('.');
        let module = parts.next().expect("object name does not have a module");
        let object_name = parts.next().expect("object is not a full name");

        println!("Looking up {object_name}");

        let linker = if module == "None" {
            self.linker_by_export_name_mut(object_name)
                .expect("failed to find linker by export name -- these should be loaded by now")
        } else if let Some(linker) = self.linker(module) {
            linker
        } else {
            self.load_linker::<E, _>(module.to_owned(), reader)?;

            self.linker(module).expect("failed to force load linker")
        };

        let linker_inner = linker.borrow_mut();
        let (export_index, _) = linker_inner
            .find_export_by_name(object_name)
            .expect("failed to find export");

        drop(linker_inner);

        self.load_object_by_export_index::<E, _>(export_index, linker, reader)
    }

    fn deserialize_object<E, R>(
        &mut self,
        object: Rc<RefCell<dyn UnrealObject>>,
        linker: Rc<RefCell<Linker>>,
        reader: &mut R,
    ) -> io::Result<()>
    where
        R: LinRead,
        E: ByteOrder,
    {
        macro_rules! deserialize_as {
            ($kind:ty) => {{
                let mut object = object.borrow_mut();

                let concrete_ty = object
                    .as_any_mut()
                    .downcast_mut::<$kind>()
                    .unwrap_or_else(|| panic!("failed to cast to {}", stringify!($kind)));

                concrete_ty.deserialize::<E, _>(self, linker, reader)
            }};
        }

        let object_kind = object.borrow().kind();
        match object_kind {
            UObjectKind::Object => {
                deserialize_as!(Object)
            }
            UObjectKind::Struct => todo!("struct!"),
            UObjectKind::Class => {
                deserialize_as!(Class)
            }
            UObjectKind::State => {
                deserialize_as!(State)
            }
            UObjectKind::Field => {
                deserialize_as!(Field)
            }
        }
    }
}
