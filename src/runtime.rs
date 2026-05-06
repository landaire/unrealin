use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
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

#[derive(Default, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct RcUnrealObjPointer(usize);
impl RcUnrealObjPointer {
    pub fn from_unreal_object(obj: &RcUnrealObject) -> Self {
        RcUnrealObjPointer(obj.as_ptr().expose_provenance())
    }
}

pub struct UnrealRuntime {
    pub linkers: HashMap<String, RcLinker>,
    /// Objects currently between `seek+ClearFlags(RF_NeedLoad)` and end of
    /// serialize body. Mirrors UE2's `RF_Preloading`. Tracked on the runtime
    /// so we can entry-check without borrowing the `RefCell` (avoids
    /// conflicting with the `borrow_mut()` held by the active `deserialize`).
    pub objects_full_loading: HashSet<RcUnrealObjPointer>,
    /// Objects whose preload has completed. Acts as the inverse of UE2's
    /// `RF_NeedLoad`. Tracked here for the same borrow-avoidance reason.
    pub loaded_objects: HashSet<RcUnrealObjPointer>,
    /// Maps a package name (e.g. "Engine") to its `len` from the LIN file table.
    /// The game's per-package reader treats this as the position the reader is at
    /// once construction is finished, so we seed `Linker::reader_offset` with it.
    pub package_file_size: HashMap<String, u64>,
    /// Mirror of UE2's `GObjLoaded`: every newly constructed object lands here so
    /// `end_load` can drain it in serial-offset order. That's the "random I/O ->
    /// sequential bytes" property the .lin format relies on.
    pub pending_loads: Vec<RcUnrealObject>,
    /// Mirror of `GObjBeginLoadCount`. Reference-counted; only when it drops back
    /// to 0 does `end_load` actually process the queue.
    pub begin_load_count: u32,
    /// Monotonic counter handed to each newly-constructed object. The engine's
    /// EndLoad sort is by `(ULinker*, serial_offset)` where `ULinker*` is heap
    /// address; in our heap that doesn't match construction order naturally, so
    /// we attach an explicit construction index and sort by that.
    pub next_construction_index: u64,
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

        let mut linker = Linker::new(expected_name.clone(), package);
        if let Some(size) = self.package_file_size.get(&expected_name) {
            linker.set_position(*size);
        }

        self.linkers
            .insert(expected_name, Rc::new(RefCell::new(linker)));

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

    pub fn full_load_object<E, R>(&mut self, obj: &RcUnrealObject, reader: &mut R) -> io::Result<()>
    where
        E: ByteOrder,
        R: LinRead,
    {
        self.preload::<E, _>(obj, reader)
    }

    pub fn begin_load(&mut self) {
        self.begin_load_count += 1;
    }

    pub fn end_load<E, R>(&mut self, reader: &mut R) -> io::Result<()>
    where
        E: ByteOrder,
        R: LinRead,
    {
        if self.begin_load_count > 0 {
            self.begin_load_count -= 1;
        }
        if self.begin_load_count > 0 {
            return Ok(());
        }

        // Splinter Cell's `EndLoad` (verified by decompilation of the SC xbe at
        // 0x48d80) processes ObjLoaded in *insertion order*. There is no
        // `Sort(...)` call like UT2004 has. New items appended during a
        // `Preload(...)` go to the back of the same array and are processed
        // after the current items. We match that with a FIFO drain that takes
        // each batch in insertion order.
        let mut iter_no = 0;
        loop {
            let batch: Vec<RcUnrealObject> = std::mem::take(&mut self.pending_loads);
            if batch.is_empty() {
                break;
            }
            tracing::info!(
                "end_load iter {}: {} item(s) (FIFO)",
                iter_no,
                batch.len()
            );
            for (i, obj) in batch.iter().enumerate().take(20) {
                let inner = obj.borrow();
                let base = inner.base_object();
                let linker = base.linker();
                let linker_inner = linker.borrow();
                if let Some(export) = linker_inner.find_export_by_index(base.export_index()) {
                    tracing::info!(
                        "  [{}] {} (offset={:#X}, size={:#X}, needs_load={})",
                        i,
                        export.full_name(&linker_inner),
                        export.serial_offset(),
                        export.serial_size(),
                        base.needs_load()
                    );
                }
            }
            for obj in batch {
                self.preload::<E, _>(&obj, reader)?;
            }
            iter_no += 1;
        }
        Ok(())
    }

    fn preload<E, R>(&mut self, obj: &RcUnrealObject, reader: &mut R) -> io::Result<()>
    where
        E: ByteOrder,
        R: LinRead,
    {
        // Mirror UE2's `ULinkerLoad::Preload` (Core/Src/UnLinker.cpp). Tracked
        // entirely on the runtime so the entry gate doesn't need to borrow
        // the `RefCell` (the active `deserialize_object` already holds a
        // mutable borrow on this same object during its tag-value loop).
        //
        // `loaded_objects` is the inverse of UE2 `RF_NeedLoad`, set after
        // the serialize body completes. `objects_full_loading` is UE2
        // `RF_Preloading`, set across the seek+serialize span. Recursive
        // `Preload(this)` from inside the serialize body short-circuits
        // here, matching the engine where `RF_NeedLoad` was just cleared
        // before `Object->Serialize(*this)`.
        let pointer_value = RcUnrealObjPointer::from_unreal_object(obj);
        if self.loaded_objects.contains(&pointer_value)
            || self.objects_full_loading.contains(&pointer_value)
        {
            return Ok(());
        }

        let (linker, export_index, is_struct) = {
            let inner = obj.borrow();
            let base = inner.base_object();
            (
                base.linker(),
                base.export_index(),
                inner.is_a(UObjectKind::Struct),
            )
        };
        let export = linker
            .borrow()
            .find_export_by_index(export_index)
            .expect("preload: missing export")
            .clone();

        if is_struct && export.super_index != 0 {
            let super_obj = self.load_object_by_raw_index::<E, _>(
                export.super_index,
                &linker,
                LoadKind::Create,
                reader,
            )?;
            if let Some(super_obj) = super_obj {
                self.preload::<E, _>(&super_obj, reader)?;
            }
        }

        let span = span!(
            Level::DEBUG,
            "preload",
            object_name = export.full_name(&linker.borrow()),
        );
        let _enter = span.enter();

        reader.push_linker(Rc::clone(&linker));
        let saved_pos = reader.stream_position()?;
        reader.seek(SeekFrom::Start(export.serial_offset()))?;

        self.objects_full_loading.insert(pointer_value);

        // Capture the raw bytes consumed for this export. Nested
        // preloads push their own frames so they don't pollute ours;
        // we get only this object's body.
        reader.push_capture();
        deserialize_object::<E, _>(self, Rc::clone(obj), &linker, reader)?;
        let body_bytes = reader.pop_capture();
        linker
            .borrow_mut()
            .captured
            .bodies
            .insert(export_index.0, body_bytes);

        self.objects_full_loading.remove(&pointer_value);
        self.loaded_objects.insert(pointer_value);
        // Keep `obj.needs_load` in sync for any callers that read it directly
        // (test helpers, ustruct's full_load_object loop). The runtime sets
        // are the source of truth for preload re-entry decisions.
        obj.borrow_mut().base_object_mut().loaded();

        let current_pos = reader.stream_position()?;
        let read_size = (current_pos - export.serial_offset()) as usize;
        assert_eq!(
            read_size,
            export.serial_size(),
            "Data read for export does not match expected. Read {read_size:#X} bytes, expected {:#X}",
            export.serial_size()
        );

        reader.seek(SeekFrom::Start(saved_pos))?;
        reader.pop_linker();

        Ok(())
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
    ) -> io::Result<Option<RcUnrealObject>>
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
        } else if raw_index < 0 {
            let import_index = ImportIndex::from_raw(raw_index);

            // Grab this import's linker
            let linker_inner = linker.borrow();
            let import = linker_inner
                .find_import_by_index(import_index)
                .expect("failed to find import");
            let import_full_name = import.full_name(&linker_inner, import_index);
            let class_name = import.class_name(&linker_inner).to_owned();
            let class_package = linker_inner.package.names[import.class_package as usize]
                .name
                .clone();

            drop(linker_inner);

            self.load_object_by_full_name_with_class::<E, _>(
                import_full_name.as_str(),
                Some((class_name.as_str(), class_package.as_str())),
                load_kind,
                reader,
            )
        } else {
            Ok(None)
        }
    }

    /// Loads and deserializes an object and its depencies by the export index.
    ///
    /// Returns `Ok(None)` when the export's `ObjectFlags & CONTEXT_FLAGS == 0`,
    /// matching SC's `ULinkerLoad::CreateExport` early return. The engine never
    /// *constructs* editor-only exports (e.g. `Core.ScriptText`) on Xbox.
    pub fn load_object_by_export_index<E, R>(
        &mut self,
        export_index: ExportIndex,
        linker: &Rc<RefCell<Linker>>,
        load_kind: LoadKind,
        reader: &mut R,
    ) -> io::Result<Option<RcUnrealObject>>
    where
        R: LinRead,
        E: ByteOrder,
    {
        debug!("Linker count: {}", self.linkers.len());
        for (name, linker) in self.linkers.iter() {
            debug!(
                "Linker {name} object count: {}",
                linker.borrow().objects.len()
            );
        }

        let linker_inner = linker.borrow();

        let export = linker_inner
            .find_export_by_index(export_index)
            .expect("could not find export")
            .clone();
        let export_full_name = export.full_name(&linker_inner);
        let class_name = export.class_name(&linker_inner).to_string();

        let span = span!(
            Level::INFO,
            "load_object_by_export_index",
            object_name = &export_full_name,
            load_kind = format!("{:?}", load_kind),
        );
        let _enter = span.enter();

        trace!(
            "Loading with load kind: {:?}, linker= {:#X}",
            load_kind,
            linker.as_ptr().expose_provenance()
        );

        // Check if this object has already been loaded
        let obj = if let Some(loaded_obj) = linker_inner.objects.get(&export_index) {
            trace!("Using pre-constructed {export_full_name} object");

            let obj = Rc::clone(loaded_obj);
            drop(linker_inner);

            obj
        } else {
            // Object has not yet been loaded

            trace!("({class_name}) {export_full_name} {export:#X?}");

            // Mirror SC's `CreateExport` context-flag gate: the engine never
            // *constructs* an export whose `ObjectFlags & _ContextFlags == 0`.
            // For the SC Xbox build `_ContextFlags = LOAD_FOR_CLIENT (0x10000)`.
            // Editor-only objects (e.g. `Core.ScriptText`) have `LOAD_FOR_EDIT`
            // but not `LOAD_FOR_CLIENT`, so the engine returns NULL here.
            const CONTEXT_FLAGS: ObjectFlags = ObjectFlags::LOAD_FOR_CLIENT;
            let export_flags = ObjectFlags::from_bits(export.object_flags)
                .expect("failed to construct ObjectFlags");
            if !export_flags.intersects(CONTEXT_FLAGS) {
                debug!(
                    "Skipping construction of {} (flags {:#X} lack CONTEXT_FLAGS)",
                    export_full_name, export.object_flags
                );
                return Ok(None);
            }

            debug!(
                "Entering construction branch: {}, class = {}",
                export_full_name, class_name
            );
            let object_kind = UObjectKind::try_from(export.class_name(&linker_inner))
                .unwrap_or_else(|_| panic!("could not find object kind {}", class_name));

            trace!("Resolved object kind: {object_kind:?}");

            let constructed_object = object_kind.construct(Rc::downgrade(linker), export_index);
            let construction_index = self.next_construction_index;
            self.next_construction_index += 1;
            let mut object = constructed_object.borrow_mut();
            object.base_object_mut().set_flags(
                ObjectFlags::from_bits(export.object_flags)
                    .expect("failed to construct ObjectFlags"),
            );
            object
                .base_object_mut()
                .set_name(export.object_name(&linker_inner).to_owned());
            object
                .base_object_mut()
                .set_concrete_obj(Rc::downgrade(&constructed_object));
            object
                .base_object_mut()
                .set_construction_index(construction_index);

            let class_index = export.class_index;
            let is_struct = object.is_a(UObjectKind::Struct);

            // Drop the mutable borrow before potential recursive calls
            drop(object);
            drop(linker_inner);

            let contains_key = linker.borrow().objects.contains_key(&export_index);

            // Mirror UE2's `CreateExport`: construct the class, then `Preload`
            // it synchronously. The recursive super-preload inside `preload`
            // populates the queue with the class chain ahead of any sibling
            // exports, which is what determines the EndLoad serial-offset
            // ordering on disk.
            if class_index != 0 {
                trace!("Loading class...");
                let class_obj = self.load_object_by_raw_index::<E, _>(
                    class_index,
                    linker,
                    LoadKind::Create,
                    reader,
                )?;
                if let Some(class_obj) = class_obj {
                    self.preload::<E, _>(&class_obj, reader)?;
                }
            }

            let parent = self.load_object_by_raw_index::<E, _>(
                export.package_index,
                linker,
                LoadKind::Create,
                reader,
            )?;

            let object_parsed_by_parent = linker.borrow().objects.get(&export_index).map(Rc::clone);
            if !contains_key && object_parsed_by_parent.is_some() {
                panic!("DOES CONTAIN OBJECT");
                // return Ok(obj);
            }

            if let Some(parent) = parent {
                constructed_object
                    .borrow_mut()
                    .base_object_mut()
                    .set_outer_object(parent);
            }

            let returned_obj = if let Some(obj) = object_parsed_by_parent {
                obj
            } else {
                // This is the equivalent of `GObjLoaded.AddItem(this)` in SC's
                // `ULinkerLoad::CreateExport` (xbe `0x39512`): only fires after
                // the class chain has been preloaded and the outer chain has
                // been constructed. Logging here matches the QEMU plugin's
                // captured `gobj_loaded_order`.
                info!(
                    "Constructing new object: {}, class = {}",
                    export_full_name, class_name
                );
                linker
                    .borrow_mut()
                    .objects
                    .insert(export_index, Rc::clone(&constructed_object));
                self.pending_loads.push(Rc::clone(&constructed_object));

                constructed_object
            };

            // Ensure super class is loaded.
            if is_struct && export.super_index != 0 {
                trace!("Loading super item");
                self.load_object_by_raw_index::<E, _>(
                    export.super_index,
                    linker,
                    LoadKind::Create,
                    reader,
                )?;
                trace!("Super item loaded");
            }

            returned_obj
        };

        match load_kind {
            LoadKind::Create => {
                debug!("Returning -- object was constructed with LoadKind::Create");
            }
            LoadKind::Full | LoadKind::Load => {
                // Deserialize is deferred to `end_load`. The object is in
                // `pending_loads` either from this call's construction branch or
                // from an earlier construction; the queue drain in `end_load`
                // will Preload it in serial-offset order.
                debug!(
                    "Queued for end_load: {} (class = {})",
                    export_full_name, class_name
                );
            }
        }

        Ok(Some(obj))
    }

    pub fn load_object_by_full_name<E, R>(
        &mut self,
        full_name: &str,
        load_kind: LoadKind,
        reader: &mut R,
    ) -> io::Result<Option<RcUnrealObject>>
    where
        R: LinRead,
        E: ByteOrder,
    {
        self.load_object_by_full_name_with_class::<E, _>(full_name, None, load_kind, reader)
    }

    pub fn load_object_by_full_name_with_class<E, R>(
        &mut self,
        full_name: &str,
        class_info: Option<(&str, &str)>,
        load_kind: LoadKind,
        reader: &mut R,
    ) -> io::Result<Option<RcUnrealObject>>
    where
        R: LinRead,
        E: ByteOrder,
    {
        let mut parts = full_name.split('.');
        let module = parts.next().expect("object name does not have a module");
        let object_name = parts.next().expect("object is not a full name");

        let span = span!(
            Level::DEBUG,
            "load_object_by_full_name",
            name = full_name,
            load_kind = format!("{:?}", load_kind)
        );
        let _enter = span.enter();

        debug!("Looking up {full_name}");

        if module == "Core"
            && let Ok(kind) = UObjectKind::try_from(object_name)
        {
            debug!("Object is a builtin of kind {kind:?}");

            return Ok(None);
        }

        let linker = if module == "None" {
            self.linker_by_export_name_mut(object_name)
                .expect("failed to find linker by export name -- these should be loaded by now")
        } else if let Some(linker) = self.linker(module) {
            linker
        } else {
            self.load_linker::<E, _>(module.to_owned(), reader)?;

            self.linker(module).expect("failed to force load linker")
        };

        let linker_inner = linker.borrow();
        let lookup = if let Some((cn, cp)) = class_info {
            linker_inner.find_export_by_name_and_class(object_name, cn, cp)
        } else {
            linker_inner.find_export_by_name(object_name)
        };
        let Some((export_index, _)) = lookup else {
            drop(linker_inner);
            tracing::warn!(
                "unresolved import {full_name:?} (likely a native class); returning None"
            );
            return Ok(None);
        };

        drop(linker_inner);

        self.load_object_by_export_index::<E, _>(export_index, &linker, load_kind, reader)
    }
}
