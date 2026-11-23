use crate::{
    trap::drain_stack_pool, vmcontext::VMMemoryDefinition, LinearMemory, VMExceptionObj,
    VMExternObj, VMFunction, VMFunctionEnvironment, VMGlobal, VMInstance, VMMemory, VMTable, VMTag,
};
use core::slice::Iter;
use std::{
    cell::UnsafeCell, fmt, marker::PhantomData, mem, num::NonZeroUsize, ptr::NonNull, sync::OnceLock,
};
use wasmer_types::StoreId;

/// Trait to represent an object managed by a context. This is implemented on
/// the VM types managed by the context.
pub trait StoreObject: Sized {
    /// List the objects in the store.
    fn list(ctx: &StoreObjects) -> &Vec<Self>;

    /// List the objects in the store, mutably.
    fn list_mut(ctx: &mut StoreObjects) -> &mut Vec<Self>;
}

macro_rules! impl_context_object {
    ($($field:ident => $ty:ty,)*) => {
        $(
            impl StoreObject for $ty {
                fn list(ctx: &StoreObjects) -> &Vec<Self> {
                    &ctx.$field
                }
                fn list_mut(ctx: &mut StoreObjects) -> &mut Vec<Self> {
                    &mut ctx.$field
                }
            }
        )*
    };
}

impl_context_object! {
    functions => VMFunction,
    tables => VMTable,
    globals => VMGlobal,
    instances => VMInstance,
    memories => VMMemory,
    extern_objs => VMExternObj,
    exceptions => VMExceptionObj,
    tags => VMTag,
    function_environments => VMFunctionEnvironment,
}

/// Set of objects managed by a context.
#[derive(Debug, Default)]
pub struct StoreObjects {
    id: StoreId,
    memories: Vec<VMMemory>,
    tables: Vec<VMTable>,
    globals: Vec<VMGlobal>,
    functions: Vec<VMFunction>,
    instances: Vec<VMInstance>,
    extern_objs: Vec<VMExternObj>,
    exceptions: Vec<VMExceptionObj>,
    tags: Vec<VMTag>,
    function_environments: Vec<VMFunctionEnvironment>,
}

impl StoreObjects {
    /// Create a new instance of [`Self`]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: StoreId,
        memories: Vec<VMMemory>,
        tables: Vec<VMTable>,
        globals: Vec<VMGlobal>,
        functions: Vec<VMFunction>,
        instances: Vec<VMInstance>,
        extern_objs: Vec<VMExternObj>,
        exceptions: Vec<VMExceptionObj>,
        tags: Vec<VMTag>,
        function_environments: Vec<VMFunctionEnvironment>,
    ) -> Self {
        Self {
            id,
            memories,
            tables,
            globals,
            functions,
            instances,
            extern_objs,
            function_environments,
            exceptions,
            tags,
        }
    }

    /// Returns the ID of this context.
    pub fn id(&self) -> StoreId {
        self.id
    }

    /// Sets the ID of this store
    pub fn set_id(&mut self, id: StoreId) {
        self.id = id;
    }

    /// Returns a pair of mutable references from two handles.
    ///
    /// Panics if both handles point to the same object.
    pub fn get_2_mut<T: StoreObject>(
        &mut self,
        a: InternalStoreHandle<T>,
        b: InternalStoreHandle<T>,
    ) -> (&mut T, &mut T) {
        assert_ne!(a.index(), b.index());
        let list = T::list_mut(self);
        if a.index() < b.index() {
            let (low, high) = list.split_at_mut(b.index());
            (&mut low[a.index()], &mut high[0])
        } else {
            let (low, high) = list.split_at_mut(a.index());
            (&mut high[0], &mut low[a.index()])
        }
    }

    /// Return an immutable iterator over all globals
    pub fn iter_globals(&self) -> Iter<'_, VMGlobal> {
        self.globals.iter()
    }

    /// Return an vector of all globals and converted to u128
    pub fn as_u128_globals(&self) -> Vec<u128> {
        self.iter_globals()
            .map(|v| unsafe { v.vmglobal().as_ref().val.u128 })
            .collect()
    }

    /// Set a global, at index idx. Will panic if idx is out of range
    /// Safety: the caller should check that the raw value is compatible
    /// with destination VMGlobal type
    pub fn set_global_unchecked(&self, idx: usize, val: u128) {
        assert!(idx < self.globals.len());
        unsafe {
            self.globals[idx].vmglobal().as_mut().val.u128 = val;
        }
    }

    /// Force-unmap all linear memory backing stores before dropping handles.
    pub fn release_all_memory_mappings(&mut self) {
        for mem in &mut self.memories {
            let _ = mem.reset();
        }
    }

    /// Drops every object held in this store arena, reclaiming instance memory,
    /// linear memories, tables, globals, etc.
    pub fn clear(&mut self) {
        self.release_all_memory_mappings();
        drain_stack_pool();
        self.memories.clear();
        // Vec::clear drops the elements but keeps the allocation. A leftover
        // StoreRef would then hold that capacity until the Store itself dies.
        // take() frees the buffer here, in wasm_store_delete.
        drop(mem::take(&mut self.tables));
        drop(mem::take(&mut self.globals));
        drop(mem::take(&mut self.functions));
        self.instances.clear();
        self.extern_objs.clear();
        self.exceptions.clear();
        self.tags.clear();
        drop(mem::take(&mut self.function_environments));
    }

    /// Drops instance-owned store objects (memories/tables/globals/tags) and
    /// removes the [`VMInstance`] when it is the last entry in the arena.
    ///
    /// When the instance is not last, child resources are still dropped but the
    /// `VMInstance` shell remains (stable handle indices). Destroy instances in
    /// reverse creation order for full shell removal.
    pub fn destroy_instance(&mut self, handle: InternalStoreHandle<VMInstance>) {
        let idx = handle.index();
        if idx == 0 || idx > self.instances.len() {
            return;
        }
        let slot = idx - 1;

        // Collect handles first to avoid overlapping & / &mut borrows on self.
        let (memories, tables, globals, tags) = self.instances[slot].owned_store_handles();
        for h in memories {
            let idx = h.index();
            if idx > 0 && idx <= self.memories.len() {
                let _ = self.memories[idx - 1].reset();
            }
            self.drop_memory(h);
        }
        for h in tables {
            self.drop_table(h);
        }
        for h in globals {
            self.drop_global(h);
        }
        for h in tags {
            self.drop_tag(h);
        }

        if slot + 1 == self.instances.len() {
            drop(self.instances.pop().expect("instance slot"));
        }
        drain_stack_pool();
    }

    /// Replace and drop a [`VMMemory`] at `handle`, leaving a minimal tombstone.
    pub fn drop_memory(&mut self, handle: InternalStoreHandle<VMMemory>) {
        drop_store_object_at(&mut self.memories, handle, tombstone_memory);
    }

    /// Replace and drop a [`VMTable`] at `handle`, leaving a minimal tombstone.
    pub fn drop_table(&mut self, handle: InternalStoreHandle<VMTable>) {
        drop_store_object_at(&mut self.tables, handle, tombstone_table);
    }

    /// Replace and drop a [`VMGlobal`] at `handle`, leaving a minimal tombstone.
    pub fn drop_global(&mut self, handle: InternalStoreHandle<VMGlobal>) {
        drop_store_object_at(&mut self.globals, handle, tombstone_global);
    }

    /// Replace and drop a [`VMTag`] at `handle`, leaving a minimal tombstone.
    pub fn drop_tag(&mut self, handle: InternalStoreHandle<VMTag>) {
        drop_store_object_at(&mut self.tags, handle, tombstone_tag);
    }
}

fn drop_store_object_at<T: StoreObject>(
    list: &mut Vec<T>,
    handle: InternalStoreHandle<T>,
    tombstone: fn() -> T,
) {
    let idx = handle.index();
    if idx == 0 || idx > list.len() {
        return;
    }
    let slot = idx - 1;
    let old = std::mem::replace(&mut list[slot], tombstone());
    drop(old);
}

fn tombstone_memory() -> VMMemory {
    #[derive(Debug)]
    struct TombstoneLinearMemory(UnsafeCell<VMMemoryDefinition>);

    unsafe impl Send for TombstoneLinearMemory {}
    unsafe impl Sync for TombstoneLinearMemory {}

    impl LinearMemory for TombstoneLinearMemory {
        fn ty(&self) -> wasmer_types::MemoryType {
            wasmer_types::MemoryType::new(wasmer_types::Pages(0), None, false)
        }

        fn size(&self) -> wasmer_types::Pages {
            wasmer_types::Pages(0)
        }

        fn style(&self) -> wasmer_types::MemoryStyle {
            wasmer_types::MemoryStyle::Dynamic {
                offset_guard_size: 0,
            }
        }

        fn grow(&mut self, delta: wasmer_types::Pages) -> Result<wasmer_types::Pages, wasmer_types::MemoryError> {
            Err(wasmer_types::MemoryError::CouldNotGrow {
                current: wasmer_types::Pages(0),
                attempted_delta: delta,
            })
        }

        fn reset(&mut self) -> Result<(), wasmer_types::MemoryError> {
            Ok(())
        }

        fn vmmemory(&self) -> NonNull<VMMemoryDefinition> {
            NonNull::new(self.0.get()).unwrap()
        }

        fn try_clone(
            &self,
        ) -> Result<Box<dyn LinearMemory + Send + Sync + 'static>, wasmer_types::MemoryError> {
            Err(wasmer_types::MemoryError::MemoryNotShared)
        }

        fn copy(
            &self,
        ) -> Result<Box<dyn LinearMemory + Send + Sync + 'static>, wasmer_types::MemoryError> {
            Ok(Box::new(TombstoneLinearMemory(UnsafeCell::new(VMMemoryDefinition {
                base: std::ptr::null_mut(),
                current_length: 0,
            }))))
        }
    }

    VMMemory(Box::new(TombstoneLinearMemory(UnsafeCell::new(VMMemoryDefinition {
        base: std::ptr::null_mut(),
        current_length: 0,
    }))))
}

fn tombstone_table() -> VMTable {
    use wasmer_types::{TableStyle, TableType, Type};
    let ty = TableType::new(Type::FuncRef, 0, None);
    VMTable::new(&ty, &TableStyle::CallerChecksSignature).expect("tombstone table")
}

fn tombstone_global() -> VMGlobal {
    use wasmer_types::{GlobalType, Mutability, Type};
    VMGlobal::new(GlobalType::new(Type::I32, Mutability::Const))
}

fn tombstone_tag() -> VMTag {
    use crate::export::VMTag;
    use wasmer_types::{FunctionType, TagKind};
    static TOMBSTONE: OnceLock<VMTag> = OnceLock::new();
    TOMBSTONE
        .get_or_init(|| VMTag::new(TagKind::Exception, FunctionType::new(vec![], vec![])))
        .clone()
}

/// Handle to an object managed by a context.
///
/// Internally this is just an integer index into a context. A reference to the
/// context must be passed in separately to access the actual object.
#[cfg_attr(feature = "artifact-size", derive(loupe::MemoryUsage))]
pub struct StoreHandle<T> {
    id: StoreId,
    internal: InternalStoreHandle<T>,
}

impl<T> Clone for StoreHandle<T> {
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            internal: self.internal,
        }
    }
}

impl<T> std::hash::Hash for StoreHandle<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
        self.internal.idx.hash(state);
    }
}

impl<T: StoreObject> fmt::Debug for StoreHandle<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoreHandle")
            .field("id", &self.id)
            .field("internal", &self.internal.index())
            .finish()
    }
}

impl<T: StoreObject> PartialEq for StoreHandle<T> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && self.internal == other.internal
    }
}

impl<T: StoreObject> Eq for StoreHandle<T> {}

impl<T: StoreObject> StoreHandle<T> {
    /// Moves the given object into a context and returns a handle to it.
    pub fn new(ctx: &mut StoreObjects, val: T) -> Self {
        Self {
            id: ctx.id,
            internal: InternalStoreHandle::new(ctx, val),
        }
    }

    /// Returns a reference to the object that this handle points to.
    pub fn get<'a>(&self, ctx: &'a StoreObjects) -> &'a T {
        assert_eq!(self.id, ctx.id, "object used with the wrong context");
        self.internal.get(ctx)
    }

    /// Returns a mutable reference to the object that this handle points to.
    pub fn get_mut<'a>(&self, ctx: &'a mut StoreObjects) -> &'a mut T {
        assert_eq!(self.id, ctx.id, "object used with the wrong context");
        self.internal.get_mut(ctx)
    }

    /// Returns the internal handle contains within this handle.
    pub fn internal_handle(&self) -> InternalStoreHandle<T> {
        self.internal
    }

    /// Returns the ID of the context associated with the handle.
    pub fn store_id(&self) -> StoreId {
        self.id
    }

    /// Overrides the store id with a new ID
    pub fn set_store_id(&mut self, id: StoreId) {
        self.id = id;
    }

    /// Constructs a `StoreHandle` from a `StoreId` and an `InternalStoreHandle`.
    ///
    /// # Safety
    /// Handling `InternalStoreHandle` values is unsafe because they do not track context ID.
    pub unsafe fn from_internal(id: StoreId, internal: InternalStoreHandle<T>) -> Self {
        Self { id, internal }
    }
}

/// Internal handle to an object owned by the current context.
///
/// Unlike `StoreHandle` this does not track the context ID: it is only
/// intended to be used within objects already owned by a context.
#[repr(transparent)]
pub struct InternalStoreHandle<T> {
    // Use a NonZero here to reduce the size of Option<InternalStoreHandle>.
    idx: NonZeroUsize,
    marker: PhantomData<fn() -> T>,
}

#[cfg(feature = "artifact-size")]
impl<T> loupe::MemoryUsage for InternalStoreHandle<T> {
    fn size_of_val(&self, _tracker: &mut dyn loupe::MemoryUsageTracker) -> usize {
        std::mem::size_of_val(self)
    }
}

impl<T> Clone for InternalStoreHandle<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for InternalStoreHandle<T> {}

impl<T> fmt::Debug for InternalStoreHandle<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InternalStoreHandle")
            .field("idx", &self.idx)
            .finish()
    }
}
impl<T> PartialEq for InternalStoreHandle<T> {
    fn eq(&self, other: &Self) -> bool {
        self.idx == other.idx
    }
}
impl<T> Eq for InternalStoreHandle<T> {}

impl<T: StoreObject> InternalStoreHandle<T> {
    /// Moves the given object into a context and returns a handle to it.
    pub fn new(ctx: &mut StoreObjects, val: T) -> Self {
        let list = T::list_mut(ctx);
        let idx = NonZeroUsize::new(list.len() + 1).unwrap();
        list.push(val);
        Self {
            idx,
            marker: PhantomData,
        }
    }

    /// Returns a reference to the object that this handle points to.
    pub fn get<'a>(&self, ctx: &'a StoreObjects) -> &'a T {
        &T::list(ctx)[self.idx.get() - 1]
    }

    /// Returns a mutable reference to the object that this handle points to.
    pub fn get_mut<'a>(&self, ctx: &'a mut StoreObjects) -> &'a mut T {
        &mut T::list_mut(ctx)[self.idx.get() - 1]
    }

    pub(crate) fn index(&self) -> usize {
        self.idx.get()
    }

    pub(crate) fn from_index(idx: usize) -> Option<Self> {
        NonZeroUsize::new(idx).map(|idx| Self {
            idx,
            marker: PhantomData,
        })
    }
}

/// Data used by the generated code is generally located inline within the
/// `VMContext` for items defined in an instance. Host-defined objects are
/// allocated separately and owned directly by the context.
pub enum MaybeInstanceOwned<T> {
    /// The data is owned here.
    Host(Box<UnsafeCell<T>>),

    /// The data is stored inline in the `VMContext` of an instance.
    Instance(NonNull<T>),
}

impl<T> MaybeInstanceOwned<T> {
    /// Returns underlying pointer to the VM data.
    pub fn as_ptr(&self) -> NonNull<T> {
        match self {
            Self::Host(p) => unsafe { NonNull::new_unchecked(p.get()) },
            Self::Instance(p) => *p,
        }
    }
}

impl<T> std::fmt::Debug for MaybeInstanceOwned<T>
where
    T: std::fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Host(p) => {
                write!(f, "host(")?;
                p.as_ref().fmt(f)?;
                write!(f, ")")
            }
            Self::Instance(p) => {
                write!(f, "instance(")?;
                unsafe { p.as_ref().fmt(f)? };
                write!(f, ")")
            }
        }
    }
}
