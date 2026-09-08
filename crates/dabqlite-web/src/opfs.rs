//! The browser binding: real OPFS sync access handles behind
//! [`SyncHandle`], plus the async acquisition that must happen before any
//! synchronous I/O can (docs/DESIGN.md §8.1).
//!
//! Only compiled for `wasm32`, and only *usable* inside a dedicated
//! worker — `createSyncAccessHandle` exists nowhere else. That is not a
//! limitation this design has to work around: the file set is declared
//! up front, so every handle is acquired once during the async
//! [`open_dir`] and held for the session, and the engine below never
//! learns that I/O was ever asynchronous.

use alloc::format;
use alloc::string::String;

use js_sys::{Function, Object, Reflect, Uint8Array};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    FileSystemDirectoryHandle, FileSystemFileHandle, FileSystemGetDirectoryOptions,
    FileSystemGetFileOptions, FileSystemReadWriteOptions, FileSystemSyncAccessHandle,
    WorkerGlobalScope,
};

use dabqlite_core::migration::V1_SCHEMA_HASH;
use dabqlite_core::SCHEMA_HASH;
use dabqlite_host::{rows_file_name, SUPERBLOCK_FILE};

use crate::{OpfsStorage, SyncHandle};

/// A held `FileSystemSyncAccessHandle`.
pub struct OpfsHandle(FileSystemSyncAccessHandle);

impl OpfsHandle {
    /// The underlying handle, for `close()` or other direct use.
    pub fn raw(&self) -> &FileSystemSyncAccessHandle {
        &self.0
    }
}

/// **Releasing the handle releases the browser's single-writer lock.**
///
/// The lock IS the handle (§8.1), which makes it `flock` with one
/// difference that used to matter enormously: a process losing a file
/// descriptor releases `flock`, and a worker losing an `OpfsHandle`
/// released nothing at all. The lock stayed held by an object nothing
/// could reach for the life of the worker, and every retry came back
/// `NoModificationAllowedError` — which to an application is not a
/// refusal, it is "the database is gone".
///
/// Two ordinary paths hit it. A database dropped instead of closed —
/// which is every error path in every caller, because `?` does not call
/// `close()`. And a HALF-acquired database: `open_dir` takes three
/// handles in sequence, so a refusal on the second leaked the first,
/// wedging a file the failed open never used and making the retry fail
/// for a different reason than the original.
///
/// `close()` is idempotent by spec, so [`OpfsStorage::close`] still
/// works and still means "release NOW" rather than "release at the end
/// of the scope".
impl Drop for OpfsHandle {
    fn drop(&mut self) {
        self.0.close();
    }
}

/// `{ at: n }` — the positional option both `read` and `write` take.
/// Built by reflection rather than the typed setters, which have been
/// renamed across web-sys releases; the shape is fixed by the spec.
fn at_option(at: u64) -> FileSystemReadWriteOptions {
    let opts = Object::new();
    let _ = Reflect::set(
        &opts,
        &JsValue::from_str("at"),
        &JsValue::from_f64(at as f64),
    );
    opts.unchecked_into()
}

fn create_option<T: JsCast>() -> T {
    let opts = Object::new();
    let _ = Reflect::set(&opts, &JsValue::from_str("create"), &JsValue::TRUE);
    opts.unchecked_into()
}

impl SyncHandle for OpfsHandle {
    type Error = JsValue;

    fn size(&self) -> Result<u64, JsValue> {
        Ok(self.0.get_size()? as u64)
    }

    fn read_at(&self, buf: &mut [u8], at: u64) -> Result<usize, JsValue> {
        // Read into a JS-side buffer, then copy across the boundary.
        // (Handing wasm memory directly to the browser is possible but
        // makes the call sensitive to memory growth mid-call; this is
        // the boring, always-correct form.)
        let view = Uint8Array::new_with_length(buf.len() as u32);
        let n = self
            .0
            .read_with_buffer_source_and_options(&view, &at_option(at))? as usize;
        if n > 0 {
            view.subarray(0, n as u32).copy_to(&mut buf[..n]);
        }
        Ok(n)
    }

    fn write_at(&self, buf: &[u8], at: u64) -> Result<usize, JsValue> {
        let view = Uint8Array::from(buf);
        let n = self
            .0
            .write_with_buffer_source_and_options(&view, &at_option(at))?;
        Ok(n as usize)
    }

    fn flush(&self) -> Result<(), JsValue> {
        self.0.flush()
    }

    fn truncate(&self, size: u64) -> Result<(), JsValue> {
        self.0.truncate_with_f64(size as f64)
    }
}

/// Acquire the declared file set inside `dir` under the origin-private
/// filesystem root, creating anything absent.
///
/// This is the one asynchronous moment in the whole storage story, and
/// the one place a "directory" is manipulated — the same checkpoint the
/// POSIX backend uses for its `create_dir_all` + directory fsync
/// (docs/DESIGN.md §4.4).
///
/// The returned backend holds exclusive sync access handles, which is
/// the browser's single-writer lock: a second worker attempting the same
/// directory is rejected by the platform (`NoModificationAllowedError`).
pub async fn open_dir(dir: &str) -> Result<OpfsStorage<OpfsHandle>, JsValue> {
    let dir_handle = directory(dir).await?;
    let superblock = handle_for(&dir_handle, SUPERBLOCK_FILE).await?;
    let rows = handle_for(&dir_handle, &rows_file_name(SCHEMA_HASH)).await?;
    let rows_old = handle_for(&dir_handle, &rows_file_name(V1_SCHEMA_HASH)).await?;
    Ok(OpfsStorage::from_handles(superblock, rows, rows_old))
}

impl OpfsStorage<OpfsHandle> {
    /// Release every handle NOW — and with them the browser's
    /// single-writer lock, so another worker (or a later `open_dir`) can
    /// take over without waiting for this one to go out of scope.
    ///
    /// Dropping releases them too (see `Drop for OpfsHandle`); this is
    /// the explicit form, for handing a database over at a point the
    /// caller chooses rather than at the end of a binding's life.
    pub fn close(&self) {
        self.superblock.0.close();
        self.rows.0.close();
        self.rows_old.0.close();
    }
}

/// Acquire a sync access handle for ONE file in `dir`, creating it if
/// absent. `open_dir` is this three times; exposed separately because
/// the exclusive-lock behavior is worth testing directly.
pub async fn acquire(dir: &str, name: &str) -> Result<OpfsHandle, JsValue> {
    let dir_handle = directory(dir).await?;
    handle_for(&dir_handle, name).await
}

/// Delete a database directory and everything in it. Fails while any
/// handle is still open, which is the platform telling the truth about
/// who holds the lock.
pub async fn remove_dir(dir: &str) -> Result<(), JsValue> {
    let root = storage_root().await?;
    let opts = Object::new();
    let _ = Reflect::set(&opts, &JsValue::from_str("recursive"), &JsValue::TRUE);
    JsFuture::from(root.remove_entry_with_options(dir, &opts.unchecked_into())).await?;
    Ok(())
}

async fn directory(dir: &str) -> Result<FileSystemDirectoryHandle, JsValue> {
    let root = storage_root().await?;
    Ok(JsFuture::from(
        root.get_directory_handle_with_options(
            dir,
            &create_option::<FileSystemGetDirectoryOptions>(),
        ),
    )
    .await?
    .unchecked_into())
}

/// The OPFS root, from inside a dedicated worker.
async fn storage_root() -> Result<FileSystemDirectoryHandle, JsValue> {
    let global = js_sys::global();
    let scope: WorkerGlobalScope = global.dyn_into().map_err(|_| {
        JsValue::from_str(
            "dabqlite: OPFS sync access handles exist only in a dedicated \
             worker (docs/DESIGN.md §8.1) — this is not a worker scope",
        )
    })?;
    let dir = JsFuture::from(scope.navigator().storage().get_directory()).await?;
    Ok(dir.unchecked_into())
}

async fn handle_for(dir: &FileSystemDirectoryHandle, name: &str) -> Result<OpfsHandle, JsValue> {
    handle_for_in(dir, name, AccessMode::ReadWrite).await
}

async fn handle_for_in(
    dir: &FileSystemDirectoryHandle,
    name: &str,
    mode: AccessMode,
) -> Result<OpfsHandle, JsValue> {
    let file: FileSystemFileHandle = JsFuture::from(
        dir.get_file_handle_with_options(name, &create_option::<FileSystemGetFileOptions>()),
    )
    .await
    .map_err(|e| annotate(e, &format!("opening {name}")))?
    .unchecked_into();
    // `createSyncAccessHandle(options)` is reached by reflection for the
    // same reason the read/write options are: the typed binding has been
    // renamed across web-sys releases, while the shape is fixed by the
    // spec. The no-argument call and the `{ mode: "readwrite" }` call are
    // defined to be the same thing, so this has one path, not two.
    let opts = Object::new();
    let _ = Reflect::set(
        &opts,
        &JsValue::from_str("mode"),
        &JsValue::from_str(mode.as_str()),
    );
    let create: Function = Reflect::get(&file, &JsValue::from_str("createSyncAccessHandle"))?
        .dyn_into()
        .map_err(|_| JsValue::from_str("dabqlite: createSyncAccessHandle is not callable"))?;
    let promise: js_sys::Promise = create
        .call1(&file, &opts)
        .map_err(|e| annotate(e, &format!("locking {name} for {}", mode.as_str())))?
        .unchecked_into();
    let handle = JsFuture::from(promise)
        .await
        .map_err(|e| annotate(e, &format!("locking {name} for {}", mode.as_str())))?;
    Ok(OpfsHandle(handle.unchecked_into()))
}

/// The access mode a sync access handle is opened in — the browser's
/// entire multi-tab vocabulary (design §10, "multi-tab coordination").
///
/// The three modes are not three levels of permission; they are three
/// different EXCLUSION rules, and which combinations the platform grants
/// decides what a second tab can do. `dabqlite` takes `ReadWrite`, which
/// is the strongest: the platform itself guarantees one writer, the same
/// way `flock` does on POSIX (§2). The others exist here so the trade
/// can be measured against a real browser rather than argued from the
/// specification — see `opfs_browser.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessMode {
    /// Exclusive: no other handle of any mode, in any tab. The default,
    /// and the library's single-writer lock.
    ReadWrite,
    /// Shared between readers, but refused while a `ReadWrite` handle
    /// exists anywhere.
    ReadOnly,
    /// Shared between writers, with NO exclusion at all — the platform
    /// stops protecting the file and something else has to.
    ReadWriteUnsafe,
}

impl AccessMode {
    fn as_str(self) -> &'static str {
        match self {
            AccessMode::ReadWrite => "readwrite",
            AccessMode::ReadOnly => "read-only",
            AccessMode::ReadWriteUnsafe => "readwrite-unsafe",
        }
    }
}

/// `acquire`, in a chosen access mode. The measurement instrument for
/// the multi-tab question: what the platform grants, asked of the
/// platform.
pub async fn acquire_in(dir: &str, name: &str, mode: AccessMode) -> Result<OpfsHandle, JsValue> {
    let dir_handle = directory(dir).await?;
    handle_for_in(&dir_handle, name, mode).await
}

/// Keep the DOMException, add what we were doing — a bare
/// `NoModificationAllowedError` is otherwise a mystery to debug.
///
/// And name the one that is not a defect at all. A refused sync access
/// handle is overwhelmingly the SECOND TAB, not a broken browser: the
/// platform is enforcing "one writer, always" (§2) exactly as intended,
/// and a caller who is told `NoModificationAllowedError` has to already
/// know the whole OPFS locking model to work that out. A caller told
/// "another tab or worker already has this database open" can put a
/// message on the screen.
fn annotate(err: JsValue, what: &str) -> JsValue {
    let name = err
        .dyn_ref::<js_sys::Error>()
        .map(|e| String::from(e.name()))
        .unwrap_or_default();
    let message = match err.dyn_ref::<js_sys::Error>() {
        Some(e) => String::from(e.message()),
        None => format!("{err:?}"),
    };
    if name == "NoModificationAllowedError" {
        return JsValue::from_str(&format!(
            "dabqlite: {what}: another tab or worker already has this \
             database open — one writer at a time is the guarantee, and \
             the browser is enforcing it ({message})"
        ));
    }
    JsValue::from_str(&format!("dabqlite: {what}: {message}"))
}
