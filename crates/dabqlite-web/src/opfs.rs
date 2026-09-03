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

use js_sys::{Object, Reflect, Uint8Array};
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
    /// Release every handle — and with them the browser's single-writer
    /// lock, so another worker (or a later `open_dir`) can take over.
    /// Dropping without closing leaves the locks held until the worker
    /// itself goes away.
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
    let file: FileSystemFileHandle = JsFuture::from(
        dir.get_file_handle_with_options(name, &create_option::<FileSystemGetFileOptions>()),
    )
    .await
    .map_err(|e| annotate(e, &format!("opening {name}")))?
    .unchecked_into();
    let handle = JsFuture::from(file.create_sync_access_handle())
        .await
        .map_err(|e| annotate(e, &format!("locking {name}")))?;
    Ok(OpfsHandle(handle.unchecked_into()))
}

/// Keep the DOMException, add what we were doing — a bare
/// `NoModificationAllowedError` is otherwise a mystery to debug.
fn annotate(err: JsValue, what: &str) -> JsValue {
    let message = match err.dyn_ref::<js_sys::Error>() {
        Some(e) => String::from(e.message()),
        None => format!("{err:?}"),
    };
    JsValue::from_str(&format!("dabqlite: {what}: {message}"))
}
