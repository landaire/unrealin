use std::{
    cell::RefCell,
    collections::HashMap,
    io::{self, SeekFrom},
    rc::Rc,
};

use byteorder::ByteOrder;
use tracing::{Level, debug, info, span, trace};

use crate::object::{DeserializeUnrealObject, RcUnrealObject, deserialize_object};
use crate::{
    de::{ExportIndex, ImportIndex, Linker, read_package},
    object::builtins::*,
    object::{ObjectFlags, UObjectKind, UnrealObject},
    reader::LinRead,
};

type RcLinker = Rc<RefCell<Linker>>;

pub struct UnrealRuntime {
    pub linkers: HashMap<String, RcLinker>,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum LoadKind {
    Load,
    Create,
    Full,
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

        let linker = Rc::new(RefCell::new(Linker::new(expected_name.clone(), package)));
        let linker_inner = linker.borrow();

        for export in &linker_inner.package.exports {
            if export.serial_offset == 0x63BA {
                panic!(
                    "{} {}",
                    export.full_name(&linker_inner),
                    export.class_name(&linker_inner)
                );
            }
        }

        drop(linker_inner);

        self.linkers.insert(expected_name, linker);

        Ok(())
    }

    fn linker(&self, name: &str) -> Option<RcLinker> {
        self.linkers.get(name).map(Rc::clone)
    }

    fn find_object(&self, name: &str) -> Option<RcUnrealObject> {
        self.linkers.values().find_map(|linker| {
            linker
                .borrow()
                .objects
                .values()
                .find(|obj| obj.borrow().base_object().name() == name)
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

    /// Loads an object by its raw encoded index. If the index refers to an import, the import will be returned.
    /// If the object refers to an export, the export will be returned.
    ///
    /// If the object has not yet been loaded, it and its dependencies will be loaded.
    ///
    /// Can return `None` if the index is 0.
    pub fn load_object_by_raw_index<E, R>(
        &mut self,
        raw_index: i32,
        linker: &Rc<RefCell<Linker>>,
        load_kind: LoadKind,
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
                load_kind,
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

            self.load_object_by_full_name::<E, _>(import_full_name.as_str(), load_kind, reader)
                .map(Some)
        } else {
            Ok(None)
        }
    }

    /// Loads and deserializes an object and its depencies by the export index.
    pub fn load_object_by_export_index<E, R>(
        &mut self,
        export_index: ExportIndex,
        linker: &Rc<RefCell<Linker>>,
        load_kind: LoadKind,
        reader: &mut R,
    ) -> io::Result<Rc<RefCell<dyn UnrealObject>>>
    where
        R: LinRead,
        E: ByteOrder,
    {
        let span = span!(Level::INFO, "load_object_by_export_index");
        let _enter = span.enter();

        trace!("Loading with load kind: {:?}", load_kind);

        let linker_inner = linker.borrow();

        let export = linker_inner
            .find_export_by_index(export_index)
            .expect("could not find export");
        let export_offset = export.serial_offset();
        let export_size = export.serial_size();
        let export_full_name = export.full_name(&linker.borrow());
        let class_name = export.class_name(&linker_inner).to_string();

        // Check if this object has already been loaded
        let obj = if let Some(loaded_obj) = linker_inner.objects.get(&export_index) {
            let obj = Rc::clone(loaded_obj);
            drop(linker_inner);

            obj
        } else {
            // Object has not yet been loaded

            trace!("{:#X?}", export);

            info!(
                "Loading object: {}, class = {}",
                export_full_name, class_name
            );
            let object_kind = UObjectKind::try_from(export.class_name(&linker_inner))
                .unwrap_or_else(|_| panic!("could not find object kind {}", class_name));

            trace!("Resolved object kind: {object_kind:?}");

            let constructed_object = object_kind.construct(Rc::downgrade(linker), export_index);
            let mut object = constructed_object.borrow_mut();
            object.base_object_mut().set_flags(
                ObjectFlags::from_bits(export.object_flags)
                    .expect("failed to construct ObjectFlags"),
            );
            object
                .base_object_mut()
                .set_name(export.object_name(&linker_inner).to_owned());

            let parent_index = export.super_index;
            drop(linker_inner);
            // If this is a struct, load the dependencies
            if object.is_a(UObjectKind::Struct) && parent_index != 0 {
                // Load dependent types

                self.load_object_by_raw_index::<E, _>(
                    parent_index,
                    linker,
                    LoadKind::Full,
                    reader,
                )?;
            }

            linker
                .borrow_mut()
                .objects
                .insert(export_index, Rc::clone(&constructed_object));

            // TODO: for experimentation
            object.base_object_mut().post_loaded();

            drop(object);

            constructed_object
        };

        if obj.borrow().base_object().is_fully_loaded() {
            trace!("Object is fully loaded");

            return Ok(obj);
        }

        match load_kind {
            // LoadKind::Load => {
            //     todo!("load/post-load");
            // }
            LoadKind::Create => {
                // Nothing needs to happen here
            }
            LoadKind::Full | LoadKind::Load => {
                let saved_pos = reader.stream_position()?;
                reader.seek(SeekFrom::Start(export_offset))?;

                debug!(
                    "Deserializing {} (class = {})",
                    export_full_name, class_name
                );
                deserialize_object::<E, _>(self, Rc::clone(&obj), linker, reader)?;

                let current_pos = reader.stream_position()?;
                let read_size = (current_pos - export_offset) as usize;
                assert_eq!(
                    read_size, export_size,
                    "Data read for export does not match expected. Read {read_size:#X} bytes, expected {export_size:#X}",
                );

                reader.seek(SeekFrom::Start(saved_pos))?;

                obj.borrow_mut().base_object_mut().loaded();
            }
        }

        Ok(obj)
    }

    pub fn load_object_by_full_name<E, R>(
        &mut self,
        full_name: &str,
        load_kind: LoadKind,
        reader: &mut R,
    ) -> io::Result<Rc<RefCell<dyn UnrealObject>>>
    where
        R: LinRead,
        E: ByteOrder,
    {
        let mut parts = full_name.split('.');
        let module = parts.next().expect("object name does not have a module");
        let object_name = parts.next().expect("object is not a full name");

        println!("Looking up {full_name}");

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

        self.load_object_by_export_index::<E, _>(export_index, &linker, load_kind, reader)
    }
}
