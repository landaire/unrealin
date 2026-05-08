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
    /// Set of package names that are physically present in our sources
    /// (`common.lin` + `map.lin`). Derived from the QEMU-captured
    /// `file_load_order`. Used to gate the `VerifyAllImports`-style cascade
    /// in `load_linker`: imports may reference engine intrinsics or modules
    /// not packed in this `.lin`, and trying to `load_linker` for one of
    /// those would consume arbitrary bytes from the source as a fake header.
    pub present_packages: HashSet<String>,
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
    /// Stack of full names of preloads currently in progress. Innermost
    /// is at the top; used by `ENQ#` diagnostics to attribute newly-
    /// constructed objects to whoever's `deserialize` triggered them.
    pub preload_stack: Vec<String>,
    /// Full LIN file_table as `(lowercase_path, len)` pairs. Used for
    /// suffix-keyed lookups of non-package files (lipsynch `.bin`,
    /// language-localized assets, etc.) that the engine loads inline
    /// from a Serialize body. Populated once at file_table parse time;
    /// scanned linearly because the count is small (~3.5k entries) and
    /// lookups are rare (~hundreds of sounds per session).
    pub file_table_entries: Vec<(String, u64)>,
}

impl UnrealRuntime {
    /// Find a file in the LIN file_table whose path ends with `body_filename`
    /// (case-insensitive). Used to resolve lipsynch `.bin` references stored
    /// in `USound.lipsynch_filename`: the body holds a relative path like
    /// `\S0_0_Voice\00_25_01.bin`, and the file_table entry for it is
    /// `Lipsynch\int\S0_0_Voice\00_25_01.bin` (the engine's `FindFile`
    /// resolves the full path through localized search dirs we don't model).
    /// Returns `Some(len)` for the first suffix match, `None` if not found —
    /// matching the engine's behavior of bailing out (no read) on open
    /// failure rather than panicking.
    pub fn find_file_by_suffix(&self, body_filename: &str) -> Option<u64> {
        let normalized = body_filename
            .trim_start_matches('\\')
            .trim_start_matches('/')
            .to_lowercase();
        if normalized.is_empty() {
            return None;
        }
        for (path, size) in &self.file_table_entries {
            // file_table_entries paths are stored lowercased, so direct ends_with works.
            if path.ends_with(&normalized) {
                return Some(*size);
            }
        }
        None
    }
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
        let pkg_source_start = reader.source_consumed();
        debug!(
            "load_linker {} starting at source 0x{:X}",
            expected_name, pkg_source_start
        );
        reader.set_reading_linker_header(true);
        let package = read_package::<E, _>(reader)?;
        let pkg_source_end = reader.source_consumed();
        debug!(
            "load_linker {} ended at source 0x{:X} (consumed 0x{:X} bytes)",
            expected_name,
            pkg_source_end,
            pkg_source_end - pkg_source_start
        );

        let mut linker = Linker::new(expected_name.clone(), package);
        linker.source_start = pkg_source_start;
        if let Some(size) = self.package_file_size.get(&expected_name) {
            linker.set_position(*size);
            debug!(
                "load_linker {} reader_offset seeded to {:#X} (file_table.len)",
                expected_name, size
            );
        } else {
            // No file_table entry (typical for cascade-loaded import packages
            // that aren't top-level entries in the LIN). Seed reader_offset
            // with the post-header reader position so the first push_linker
            // for this package leaves self.pos where the engine actually
            // sits after parsing the header, not back at zero.
            let post_header_pos = reader.stream_position()?;
            linker.set_position(post_header_pos);
            debug!(
                "load_linker {} reader_offset seeded to {:#X} (post-header pos)",
                expected_name, post_header_pos
            );
        }

        let linker_rc = Rc::new(RefCell::new(linker));
        self.linkers
            .insert(expected_name, Rc::clone(&linker_rc));

        // Mirror SC's `VerifyAllImports` (xbe `0x39da0`), called at the end of
        // `ULinkerLoad::ULinkerLoad`. For every import in the new package, walk
        // the package_index chain to its top-level ancestor (the import whose
        // `package_index == 0`, i.e. a root `Core.Package` reference) and ensure
        // that package's linker is loaded. The cascade advances the underlying
        // sequential reader past every stacked package's tables, so when a
        // subsequent preload seeks into one of *this* package's exports the
        // bytes it consumes are the actual export body — not whatever stacked
        // package PKG_TAG happened to follow our header.
        //
        // Recursion safety: we insert into `self.linkers` *before* calling
        // verify_imports so cyclic import graphs short-circuit on the
        // `contains_key` check below.
        self.verify_imports::<E, _>(&linker_rc, reader)?;

        reader.set_reading_linker_header(false);

        Ok(())
    }

    fn verify_imports<E, R>(&mut self, linker: &RcLinker, reader: &mut R) -> io::Result<()>
    where
        R: LinRead,
        E: ByteOrder,
    {
        let imports_count = linker.borrow().package.imports.len();
        for i in 0..imports_count {
            let pkg_name = {
                let l = linker.borrow();
                let mut idx: usize = i;
                let mut top_name: Option<String> = None;
                loop {
                    let imp = &l.package.imports[idx];
                    if imp.package_index == 0 {
                        top_name = Some(
                            l.package.names[imp.object_name as usize].name.clone(),
                        );
                        break;
                    }
                    if imp.package_index >= 0 {
                        // Malformed parent reference; skip.
                        break;
                    }
                    idx = (-imp.package_index - 1) as usize;
                }
                top_name
            };
            let Some(pkg_name) = pkg_name else { continue };
            if pkg_name.is_empty() {
                continue;
            }
            if self.linkers.contains_key(&pkg_name) {
                continue;
            }
            // Only cascade-load packages that are physically present in our
            // sources. Imports may reference engine intrinsics or modules
            // not packed in this `.lin`; calling load_linker for one of
            // those would consume arbitrary bytes from the source as a
            // fake package header.
            if !self.present_packages.contains(&pkg_name) {
                continue;
            }
            self.load_linker::<E, _>(pkg_name, reader)?;
        }
        Ok(())
    }

    /// Load a linker by name without triggering any export preloads.
    /// Wraps `load_linker` for use from `decode_linear_file`'s bootstrap
    /// loop at known multi-`.lin` boundary points (e.g. `None.MyLevel`).
    /// Idempotent: if the linker is already loaded, returns Ok.
    pub fn force_load_linker<E, R>(
        &mut self,
        name: &str,
        reader: &mut R,
    ) -> io::Result<()>
    where
        R: LinRead,
        E: ByteOrder,
    {
        if self.linkers.contains_key(name) {
            return Ok(());
        }
        self.load_linker::<E, _>(name.to_owned(), reader)
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
        let key = self
            .linkers
            .iter()
            .find_map(|(linker_name, linker)| {
                linker
                    .borrow()
                    .find_export_by_name(name)
                    .map(|_| linker_name.clone())
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
        let preload_full_name = {
            let l = linker.borrow();
            format!("{} ({})", export.full_name(&l), export.class_name(&l))
        };
        self.preload_stack.push(preload_full_name.clone());

        // Capture the raw bytes consumed for this export. Nested
        // preloads push their own frames so they don't pollute ours;
        // we get only this object's body. The cheat-the-remainder
        // fallback below runs while the capture frame is still active
        // so its bytes also land in the captured body — required for
        // re-emit to write a serial_size that matches the original.
        reader.push_capture();
        deserialize_object::<E, _>(self, Rc::clone(obj), &linker, reader)?;

        self.objects_full_loading.remove(&pointer_value);
        self.loaded_objects.insert(pointer_value);
        obj.borrow_mut().base_object_mut().loaded();

        let current_pos = reader.stream_position()?;
        let read_size = (current_pos - export.serial_offset()) as usize;
        match read_size.cmp(&export.serial_size()) {
            std::cmp::Ordering::Equal => {}
            std::cmp::Ordering::Less => {
                // HEURISTIC (remove long-term): when a native class lacks a
                // Rust stub the Object tag loop terminates at None well
                // before serial_size; cheating the remainder keeps the
                // source cursor aligned with the engine. The principled
                // replacement is a real per-class stub that reads exactly
                // serial_size bytes via structured Ar.Serialize / Ar << X
                // calls. Until then, this fallback turns a missing stub
                // into a logged warning rather than a hard crash.
                let missing = export.serial_size() - read_size;
                let (full_name, class_name) = {
                    let l = linker.borrow();
                    (export.full_name(&l), export.class_name(&l).to_owned())
                };
                tracing::warn!(
                    "preload short-read for {} (class {}): consumed {:#X}/{:#X}, cheating {:#X} remainder",
                    full_name,
                    class_name,
                    read_size,
                    export.serial_size(),
                    missing,
                );
                let mut buf = vec![0u8; missing];
                reader.cheat(&mut buf)?;
            }
            std::cmp::Ordering::Greater => {
                // SC's `Preload` (xbe `0x38390`) does NOT bound reads by
                // `serial_size` — it calls `Precache(serial_size)` which
                // resolves to a no-op (`vtable[0x4c]` = `FArchive::Precache`
                // tail-calls `sub_101071a0`, an immediate-return). Several
                // SC native classes legitimately read past the export's
                // `serial_size`: `USound::Serialize` appends an inline
                // lipsynch `.bin` body when `+0x38 != 0`. Treat over-read
                // as expected behavior.
                let (full_name, class_name) = {
                    let l = linker.borrow();
                    (export.full_name(&l), export.class_name(&l).to_owned())
                };
                debug!(
                    "preload over-read for {} (class {}): consumed {:#X}/{:#X}, +{:#X} extra",
                    full_name,
                    class_name,
                    read_size,
                    export.serial_size(),
                    read_size - export.serial_size()
                );
            }
        }

        let body_bytes = reader.pop_capture();
        linker
            .borrow_mut()
            .captured
            .bodies
            .insert(export_index.0, body_bytes);

        reader.seek(SeekFrom::Start(saved_pos))?;
        reader.pop_linker();
        self.preload_stack.pop();

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
            // Mirror SC's `IndexToObject` (xbe `0x39620`): when the index is
            // out of range the engine logs `ImportIndex` and still calls
            // CreateImport with whatever's at `import_table[idx]`. On Xbox
            // that's whatever happens to be in memory — usually zeroed or
            // unmapped, which CreateImport silently treats as "no object".
            // Match that by returning None instead of crashing; the bogus
            // ref is what was on disk and the engine itself never panics.
            let Some(import) = linker_inner.find_import_by_index(import_index) else {
                tracing::warn!(
                    "out-of-range import index {} in {:?} (table len {}); treating as None",
                    raw_index,
                    linker_inner.name,
                    linker_inner.package.imports.len()
                );
                return Ok(None);
            };
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
            .unwrap_or_else(|| {
                panic!(
                    "could not find export {} in {} (export table len {})",
                    export_index.0,
                    linker_inner.name,
                    linker_inner.package.exports.len()
                )
            })
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
                .unwrap_or_else(|_| {
                    // No Rust stub for this UClass. Pure-script classes have
                    // no native Serialize override, so falling back to
                    // UObject::Serialize (the tagged-property loop) is
                    // correct. Native classes with overrides will fail the
                    // serial_size assertion in `preload`, which is the
                    // signal to add a stub.
                    tracing::warn!(
                        "no stub for class {}; treating {} as plain Object",
                        class_name, export_full_name
                    );
                    UObjectKind::Object
                });

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
        self.load_object_by_full_name_with_class::<E, _>(
            full_name,
            Some(("Class", "Core")),
            load_kind,
            reader,
        )
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
        let parts: Vec<&str> = full_name.split('.').collect();
        assert!(
            parts.len() >= 2,
            "object name does not have a module: {full_name:?}"
        );
        let module = parts[0];
        let path_parts = &parts[1..];
        let object_name = *path_parts.last().expect("path has no leaf");

        let span = span!(
            Level::DEBUG,
            "load_object_by_full_name",
            name = full_name,
            load_kind = format!("{:?}", load_kind)
        );
        let _enter = span.enter();

        debug!("Looking up {full_name}");

        if module == "Core"
            && path_parts.len() == 1
            && let Ok(kind) = UObjectKind::try_from(object_name)
        {
            debug!("Object is a builtin of kind {kind:?}");

            return Ok(None);
        }

        let linker = if module == "None" {
            match self.linker_by_export_name_mut(object_name) {
                Some(linker) => linker,
                None => return Ok(None),
            }
        } else if let Some(linker) = self.linker(module) {
            linker
        } else {
            self.load_linker::<E, _>(module.to_owned(), reader)?;

            self.linker(module).expect("failed to force load linker")
        };

        let linker_inner = linker.borrow();
        // Walk the full path so multi-segment names disambiguate. Name-only
        // lookup matches the first export with that name in the export table,
        // which mishandles cases like "Engine.Console" (the top-level Class)
        // vs "Engine.Engine.Console" (a ClassProperty nested in the Engine
        // class) where a property shares a leaf name with a top-level Class.
        let lookup = linker_inner
            .find_export_by_path(path_parts)
            .filter(|(_, export)| match class_info {
                Some((cn, cp)) => {
                    export.class_name(&linker_inner) == cn
                        && export.class_package(&linker_inner) == cp
                }
                None => true,
            })
            .or_else(|| match class_info {
                Some((cn, cp)) => {
                    linker_inner.find_export_by_name_and_class(object_name, cn, cp)
                }
                None => None,
            })
            .or_else(|| {
                // Any-class fallback: when the engine called StaticLoadObject
                // with a non-Core.Class static class (e.g. USkelMesh for
                // ESam.SamAMesh), the QEMU plugin doesn't capture InClass,
                // so our default Some(("Class", "Core")) over-filters. Try
                // finding the export by path/name without class filter, but
                // reject `*Property` and `Function` matches: those exist as
                // top-level exports for script reasons but aren't loadable
                // as standalone objects via StaticLoadObject. This preserves
                // SubActionFade's Class disambiguation (Class match wins
                // earlier) AND avoids `Engine.Primitive` falsely resolving
                // to a stray ObjectProperty named Primitive.
                if class_info.is_some() {
                    let candidate = linker_inner
                        .find_export_by_path(path_parts)
                        .or_else(|| linker_inner.find_export_by_name(object_name));
                    candidate.filter(|(_, export)| {
                        let cn = export.class_name(&linker_inner);
                        !cn.ends_with("Property") && cn != "Function"
                    })
                } else {
                    None
                }
            });
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
