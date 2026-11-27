use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use anyhow::{bail, ensure};
use base64::Engine as _;
use gpui::{Context, Pixels, Point};
use waku_client::{WorkspaceClient, WorkspaceOperation, WorkspaceResult};

use super::download;
use crate::app::Waku;

pub(super) const DRAG_OUT_THRESHOLD: f32 = 6.0;
const MIN_STREAM_CHUNK_BYTES: usize = 1024 * 1024;
const MAX_STREAM_CHUNK_BYTES: usize = 4 * 1024 * 1024;
const MAX_DESCRIPTOR_UTF16_UNITS: usize = 260;

#[derive(Clone, Debug)]
pub(in crate::app) struct DragOutCandidate {
    pub(super) relative_path: String,
    pub(super) absolute_path: PathBuf,
    pub(super) name: String,
    pub(super) is_dir: bool,
    pub(super) origin: Point<Pixels>,
}

pub(super) fn drag_threshold_exceeded(origin: Point<Pixels>, current: Point<Pixels>) -> bool {
    let dx = f32::from(current.x - origin.x);
    let dy = f32::from(current.y - origin.y);
    dx * dx + dy * dy >= DRAG_OUT_THRESHOLD * DRAG_OUT_THRESHOLD
}

#[cfg_attr(not(any(windows, test)), allow(dead_code))]
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct VirtualEntry {
    pub(super) descriptor_path: String,
    pub(super) source: String,
    pub(super) size: Option<u64>,
    pub(super) is_dir: bool,
    pub(super) follow_links: bool,
}

#[cfg_attr(not(any(windows, test)), allow(dead_code))]
pub(super) trait RemoteSource: Send + Sync {
    fn plan(&self) -> anyhow::Result<Vec<VirtualEntry>>;
    fn read_chunk(
        &self,
        source: &str,
        follow_links: bool,
        offset: u64,
        length: u64,
    ) -> anyhow::Result<(Vec<u8>, u64)>;
}

#[cfg_attr(not(any(windows, test)), allow(dead_code))]
pub(super) struct DaemonSource {
    workspace: Mutex<WorkspaceClient>,
    root: PathBuf,
    relative_path: String,
    name: String,
    is_dir: bool,
}

impl DaemonSource {
    #[cfg_attr(not(any(windows, test)), allow(dead_code))]
    pub(super) fn new(
        workspace: WorkspaceClient,
        root: PathBuf,
        relative_path: String,
        name: String,
        is_dir: bool,
    ) -> Self {
        Self {
            workspace: Mutex::new(workspace),
            root,
            relative_path,
            name,
            is_dir,
        }
    }

    fn client(&self) -> WorkspaceClient {
        self.workspace.lock().unwrap().clone()
    }
}

#[cfg_attr(not(any(windows, test)), allow(dead_code))]
fn validate_descriptor_path(path: &str) -> anyhow::Result<()> {
    ensure!(
        path.encode_utf16().count() < MAX_DESCRIPTOR_UTF16_UNITS,
        "{path} is too long to drag out"
    );
    Ok(())
}

impl RemoteSource for DaemonSource {
    fn plan(&self) -> anyhow::Result<Vec<VirtualEntry>> {
        download::validate_name(&self.name)?;
        validate_descriptor_path(&self.name)?;
        let workspace = self.client();
        if !self.is_dir {
            let WorkspaceResult::FileChunk { total_bytes, .. } =
                workspace.request(WorkspaceOperation::ReadFileChunk {
                    root: self.root.clone(),
                    relative_path: PathBuf::from(&self.relative_path),
                    offset: 0,
                    length: 0,
                })?
            else {
                bail!("the daemon returned an invalid file response");
            };
            return Ok(vec![VirtualEntry {
                descriptor_path: self.name.clone(),
                source: self.relative_path.clone(),
                size: Some(total_bytes),
                is_dir: false,
                follow_links: true,
            }]);
        }

        let cancel = AtomicBool::new(false);
        let (entries, _) =
            download::plan_remote_folder(&workspace, &self.root, &self.relative_path, &cancel)?;
        let mut plan = Vec::with_capacity(entries.len() + 1);
        plan.push(VirtualEntry {
            descriptor_path: self.name.clone(),
            source: self.relative_path.clone(),
            size: None,
            is_dir: true,
            follow_links: false,
        });
        for entry in entries {
            let suffix = entry.destination.to_string_lossy().replace('/', "\\");
            let descriptor_path = format!("{}\\{suffix}", self.name);
            validate_descriptor_path(&descriptor_path)?;
            plan.push(VirtualEntry {
                descriptor_path,
                source: entry.source,
                size: entry.file_bytes,
                is_dir: entry.file_bytes.is_none(),
                follow_links: false,
            });
        }
        Ok(plan)
    }

    fn read_chunk(
        &self,
        source: &str,
        follow_links: bool,
        offset: u64,
        length: u64,
    ) -> anyhow::Result<(Vec<u8>, u64)> {
        let operation = if follow_links {
            WorkspaceOperation::ReadFileChunk {
                root: self.root.clone(),
                relative_path: PathBuf::from(source),
                offset,
                length,
            }
        } else {
            WorkspaceOperation::ReadDownloadFileChunk {
                root: self.root.clone(),
                relative_path: PathBuf::from(source),
                offset,
                length,
            }
        };
        let WorkspaceResult::FileChunk { bytes, total_bytes } =
            self.client().request(operation)?
        else {
            bail!("the daemon returned an invalid file response");
        };
        let decoded = base64::engine::general_purpose::STANDARD.decode(bytes.trim())?;
        Ok((decoded, total_bytes))
    }
}

#[cfg_attr(not(any(windows, test)), allow(dead_code))]
pub(super) struct StreamReader {
    source: Arc<dyn RemoteSource>,
    path: String,
    follow_links: bool,
    expected: Option<u64>,
    offset: u64,
    buffer: Vec<u8>,
    buffer_pos: usize,
}

#[cfg_attr(not(any(windows, test)), allow(dead_code))]
impl StreamReader {
    pub(super) fn new(
        source: Arc<dyn RemoteSource>,
        path: String,
        follow_links: bool,
        expected: Option<u64>,
    ) -> Self {
        Self {
            source,
            path,
            follow_links,
            expected,
            offset: 0,
            buffer: Vec::new(),
            buffer_pos: 0,
        }
    }

    pub(super) fn position(&self) -> u64 {
        self.offset
            .saturating_sub((self.buffer.len() - self.buffer_pos) as u64)
    }

    pub(super) fn read_into(&mut self, out: &mut [u8]) -> anyhow::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        if self.buffer_pos >= self.buffer.len() {
            if self.expected == Some(self.offset) {
                return Ok(0);
            }
            let length = out
                .len()
                .clamp(MIN_STREAM_CHUNK_BYTES, MAX_STREAM_CHUNK_BYTES) as u64;
            let (mut chunk, total_bytes) =
                self.source
                    .read_chunk(&self.path, self.follow_links, self.offset, length)?;
            match self.expected {
                Some(expected) if expected != total_bytes => {
                    bail!("the file changed size while copying");
                }
                None => self.expected = Some(total_bytes),
                _ => {}
            }
            if self.offset >= total_bytes {
                return Ok(0);
            }
            if chunk.is_empty() {
                bail!("the file ended early");
            }
            let max_remaining = (total_bytes - self.offset) as usize;
            if chunk.len() > max_remaining {
                chunk.truncate(max_remaining);
            }
            self.offset += chunk.len() as u64;
            self.buffer = chunk;
            self.buffer_pos = 0;
        }
        let available = self.buffer.len() - self.buffer_pos;
        let count = available.min(out.len());
        out[..count].copy_from_slice(&self.buffer[self.buffer_pos..self.buffer_pos + count]);
        self.buffer_pos += count;
        Ok(count)
    }
}

impl Waku {
    pub(super) fn begin_right_panel_drag_out(
        &mut self,
        candidate: DragOutCandidate,
        cx: &mut Context<Self>,
    ) {
        #[cfg(not(windows))]
        {
            let _ = (candidate, cx);
        }
        #[cfg(windows)]
        {
            if self.files_drag_out_active {
                return;
            }
            let Some(root) = self.selected_workspace_path().map(Path::to_path_buf) else {
                return;
            };
            let daemon = self.selected_daemon().clone();
            let is_local = !daemon.is_remote();
            let workspace = WorkspaceClient::for_daemon(daemon);
            self.files_drag_out_active = true;
            cx.notify();
            cx.spawn(async move |this, cx| {
                let (errors_tx, errors_rx) = smol::channel::bounded::<String>(4);
                let drag_result = if is_local {
                    windows::drag_local(&candidate.absolute_path)
                } else {
                    let source = Arc::new(DaemonSource::new(
                        workspace,
                        root,
                        candidate.relative_path.clone(),
                        candidate.name.clone(),
                        candidate.is_dir,
                    ));
                    windows::drag_remote(source, errors_tx)
                };
                let _ = this.update(cx, |this, cx| {
                    this.files_drag_out_active = false;
                    this.right_panel_files_drop_target = None;
                    if let Err(error) = drag_result {
                        this.show_toast(tr!(
                            "files.drag_out_failed",
                            name = candidate.name.clone(),
                            error = format!("{error:#}")
                        ));
                    }
                    cx.notify();
                });
                while let Ok(message) = errors_rx.recv().await {
                    let _ = this.update(cx, |this, cx| {
                        this.show_toast(tr!(
                            "files.drag_out_failed",
                            name = candidate.name.clone(),
                            error = message
                        ));
                        cx.notify();
                    });
                }
            })
            .detach();
        }
    }
}

#[cfg(windows)]
mod windows {
    use std::ffi::c_void;
    use std::mem::ManuallyDrop;
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Condvar, Mutex};
    use std::thread::ThreadId;

    use anyhow::Context as _;
    use windows::Win32::Foundation::{
        DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DV_E_FORMATETC, E_FAIL, E_INVALIDARG, E_NOTIMPL,
        HGLOBAL, OLE_E_ADVISENOTSUPPORTED, S_FALSE, S_OK, STG_E_INVALIDFUNCTION,
    };
    use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL};
    use windows::Win32::System::Com::{
        CoTaskMemFree, DATADIR_GET, DVASPECT_CONTENT, FORMATETC, IAdviseSink, IDataObject,
        IDataObject_Impl, IEnumFORMATETC, IEnumSTATDATA, ISequentialStream_Impl, IStream,
        IStream_Impl, LOCKTYPE, STATFLAG, STATSTG, STGC, STGMEDIUM, STGMEDIUM_0, STGTY_STREAM,
        STREAM_SEEK, STREAM_SEEK_CUR, STREAM_SEEK_END, STREAM_SEEK_SET, TYMED_HGLOBAL,
        TYMED_ISTREAM,
    };
    use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
    use windows::Win32::System::Memory::{
        GMEM_MOVEABLE, GMEM_ZEROINIT, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
    };
    use windows::Win32::System::Ole::{DROPEFFECT_COPY, IDropSource, ReleaseStgMedium};
    use windows::Win32::UI::Shell::Common::ITEMIDLIST;
    use windows::Win32::UI::Shell::{
        BHID_DataObject, CFSTR_FILECONTENTS, CFSTR_FILEDESCRIPTORW, FD_ATTRIBUTES, FD_FILESIZE,
        FD_PROGRESSUI, FD_UNICODE, FILEDESCRIPTORW, FILEGROUPDESCRIPTORW,
        IDataObjectAsyncCapability, IDataObjectAsyncCapability_Impl,
        SHCreateShellItemArrayFromIDLists, SHCreateStdEnumFmtEtc, SHDoDragDrop, SHParseDisplayName,
    };
    use windows::core::{BOOL, HSTRING, Ref, Result as WinResult, implement, w};

    use super::{RemoteSource, StreamReader, VirtualEntry};

    struct PidlGuard(*mut ITEMIDLIST);

    impl Drop for PidlGuard {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe { CoTaskMemFree(Some(self.0.cast())) };
            }
        }
    }

    pub(super) fn local_data_object(path: &Path) -> anyhow::Result<IDataObject> {
        let wide = HSTRING::from(path);
        let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
        unsafe { SHParseDisplayName(&wide, None, &mut pidl, 0, None) }
            .with_context(|| format!("could not resolve {}", path.display()))?;
        let _pidl_guard = PidlGuard(pidl);
        let items = unsafe { SHCreateShellItemArrayFromIDLists(&[pidl as *const ITEMIDLIST]) }
            .with_context(|| format!("could not prepare {}", path.display()))?;
        let data: IDataObject = unsafe { items.BindToHandler(None, &BHID_DataObject) }
            .with_context(|| format!("could not read {}", path.display()))?;
        Ok(data)
    }

    fn do_drag_drop(data: &IDataObject) -> anyhow::Result<()> {
        match unsafe { SHDoDragDrop(None, data, None::<&IDropSource>, DROPEFFECT_COPY) } {
            Ok(_) => Ok(()),
            Err(error)
                if error.code() == DRAGDROP_S_DROP || error.code() == DRAGDROP_S_CANCEL =>
            {
                Ok(())
            }
            Err(error) => Err(error.into()),
        }
    }

    pub(super) fn drag_local(path: &Path) -> anyhow::Result<()> {
        let data = local_data_object(path)?;
        do_drag_drop(&data)
    }

    pub(super) fn drag_remote(
        source: Arc<dyn RemoteSource>,
        errors: smol::channel::Sender<String>,
    ) -> anyhow::Result<()> {
        let data_object =
            VirtualFilesDataObject::new(source, errors, Some(std::thread::current().id()));
        let data: IDataObject = data_object.into();
        do_drag_drop(&data)
    }

    struct Shared {
        source: Arc<dyn RemoteSource>,
        plan: Mutex<Option<Result<Arc<Vec<VirtualEntry>>, String>>>,
        plan_ready: Condvar,
        errors: smol::channel::Sender<String>,
        reported: AtomicBool,
        ui_thread: Option<ThreadId>,
        cf_descriptor: u16,
        cf_contents: u16,
        cf_preferred_effect: u16,
        async_mode: AtomicBool,
        in_operation: AtomicBool,
        stored: Mutex<Vec<(u16, Vec<u8>)>>,
    }

    impl Shared {
        fn new(
            source: Arc<dyn RemoteSource>,
            errors: smol::channel::Sender<String>,
            ui_thread: Option<ThreadId>,
        ) -> Arc<Self> {
            let cf_descriptor = unsafe { RegisterClipboardFormatW(CFSTR_FILEDESCRIPTORW) } as u16;
            let cf_contents = unsafe { RegisterClipboardFormatW(CFSTR_FILECONTENTS) } as u16;
            let cf_preferred_effect =
                unsafe { RegisterClipboardFormatW(w!("Preferred DropEffect")) } as u16;
            Arc::new(Self {
                source,
                plan: Mutex::new(None),
                plan_ready: Condvar::new(),
                errors,
                reported: AtomicBool::new(false),
                ui_thread,
                cf_descriptor,
                cf_contents,
                cf_preferred_effect,
                async_mode: AtomicBool::new(true),
                in_operation: AtomicBool::new(false),
                stored: Mutex::new(Vec::new()),
            })
        }

        fn is_ui_thread(&self) -> bool {
            self.ui_thread == Some(std::thread::current().id())
        }

        fn report_error(&self, message: &str) {
            if !self.reported.swap(true, Ordering::AcqRel) {
                let _ = self.errors.try_send(message.to_owned());
            }
        }

        fn wait_plan(&self) -> Result<Arc<Vec<VirtualEntry>>, windows::core::Error> {
            let mut guard = self.plan.lock().unwrap();
            while guard.is_none() {
                guard = self.plan_ready.wait(guard).unwrap();
            }
            match guard.as_ref().unwrap() {
                Ok(entries) => Ok(entries.clone()),
                Err(message) => {
                    self.report_error(message);
                    Err(E_FAIL.into())
                }
            }
        }
    }

    #[implement(IDataObject, IDataObjectAsyncCapability)]
    pub(super) struct VirtualFilesDataObject {
        shared: Arc<Shared>,
    }

    impl VirtualFilesDataObject {
        pub(super) fn new(
            source: Arc<dyn RemoteSource>,
            errors: smol::channel::Sender<String>,
            ui_thread: Option<ThreadId>,
        ) -> Self {
            let shared = Shared::new(source, errors, ui_thread);
            let bg = shared.clone();
            std::thread::spawn(move || {
                let result = bg
                    .source
                    .plan()
                    .map(Arc::new)
                    .map_err(|error| format!("{error:#}"));
                let mut guard = bg.plan.lock().unwrap();
                *guard = Some(result);
                bg.plan_ready.notify_all();
            });
            Self { shared }
        }

        fn format_etc(cf: u16, tymed: u32, lindex: i32) -> FORMATETC {
            FORMATETC {
                cfFormat: cf,
                ptd: std::ptr::null_mut(),
                dwAspect: DVASPECT_CONTENT.0,
                lindex,
                tymed,
            }
        }

        fn supports_query(&self, fmt: &FORMATETC) -> bool {
            if fmt.dwAspect != DVASPECT_CONTENT.0 {
                return false;
            }
            let tymed_hglobal = (fmt.tymed & TYMED_HGLOBAL.0 as u32) != 0;
            let tymed_istream = (fmt.tymed & TYMED_ISTREAM.0 as u32) != 0;
            if fmt.cfFormat == self.shared.cf_descriptor && tymed_hglobal {
                return true;
            }
            if fmt.cfFormat == self.shared.cf_contents && tymed_istream && fmt.lindex >= 0 {
                return true;
            }
            if fmt.cfFormat == self.shared.cf_preferred_effect && tymed_hglobal {
                return true;
            }
            if tymed_hglobal {
                let stored = self.shared.stored.lock().unwrap();
                return stored.iter().any(|(cf, _)| *cf == fmt.cfFormat);
            }
            false
        }
    }

    fn alloc_hglobal_bytes(bytes: &[u8]) -> WinResult<HGLOBAL> {
        let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, bytes.len().max(1)) }?;
        if !bytes.is_empty() {
            let ptr = unsafe { GlobalLock(handle) };
            if ptr.is_null() {
                return Err(E_FAIL.into());
            }
            unsafe {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.cast::<u8>(), bytes.len());
                let _ = GlobalUnlock(handle);
            }
        }
        Ok(handle)
    }

    fn build_descriptor_hglobal(
        entries: &[VirtualEntry],
        shared: &Shared,
    ) -> WinResult<HGLOBAL> {
        let header_bytes = std::mem::offset_of!(FILEGROUPDESCRIPTORW, fgd);
        let item_bytes = std::mem::size_of::<FILEDESCRIPTORW>();
        let total_bytes = header_bytes + entries.len() * item_bytes;
        let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, total_bytes) }?;
        let ptr = unsafe { GlobalLock(handle) }.cast::<u8>();
        if ptr.is_null() {
            return Err(E_FAIL.into());
        }

        let mut error_out: Option<windows::core::Error> = None;
        unsafe {
            ptr.cast::<u32>().write_unaligned(entries.len() as u32);
            let items_ptr = ptr.add(header_bytes).cast::<FILEDESCRIPTORW>();
            for (index, entry) in entries.iter().enumerate() {
                let mut desc = FILEDESCRIPTORW::default();
                let mut flags =
                    FD_ATTRIBUTES.0 as u32 | FD_PROGRESSUI.0 as u32 | FD_UNICODE.0 as u32;
                if entry.is_dir {
                    desc.dwFileAttributes = FILE_ATTRIBUTE_DIRECTORY.0;
                } else {
                    desc.dwFileAttributes = FILE_ATTRIBUTE_NORMAL.0;
                    if let Some(size) = entry.size {
                        flags |= FD_FILESIZE.0 as u32;
                        desc.nFileSizeHigh = (size >> 32) as u32;
                        desc.nFileSizeLow = size as u32;
                    }
                }
                desc.dwFlags = flags;
                let utf16: Vec<u16> = entry.descriptor_path.encode_utf16().collect();
                let mut file_name = [0u16; super::MAX_DESCRIPTOR_UTF16_UNITS];
                if utf16.len() >= file_name.len() {
                    shared.report_error(&format!("{} is too long to drag out", entry.descriptor_path));
                    error_out = Some(E_FAIL.into());
                    break;
                }
                file_name[..utf16.len()].copy_from_slice(&utf16);
                desc.cFileName = file_name;
                items_ptr.add(index).write_unaligned(desc);
            }
            let _ = GlobalUnlock(handle);
        }
        if let Some(err) = error_out {
            return Err(err);
        }
        Ok(handle)
    }

    #[allow(non_snake_case)]
    impl IDataObject_Impl for VirtualFilesDataObject_Impl {
        fn GetData(&self, pformatetcin: *const FORMATETC) -> WinResult<STGMEDIUM> {
            if pformatetcin.is_null() {
                return Err(E_INVALIDARG.into());
            }
            let fmt = unsafe { *pformatetcin };
            if fmt.dwAspect != DVASPECT_CONTENT.0 {
                return Err(DV_E_FORMATETC.into());
            }
            let tymed_hglobal = (fmt.tymed & TYMED_HGLOBAL.0 as u32) != 0;
            let tymed_istream = (fmt.tymed & TYMED_ISTREAM.0 as u32) != 0;

            if fmt.cfFormat == self.shared.cf_descriptor && tymed_hglobal {
                if self.shared.is_ui_thread() {
                    return Err(DV_E_FORMATETC.into());
                }
                let entries = self.shared.wait_plan()?;
                let hglobal = build_descriptor_hglobal(&entries, &self.shared)?;
                return Ok(STGMEDIUM {
                    tymed: TYMED_HGLOBAL.0 as u32,
                    u: STGMEDIUM_0 { hGlobal: hglobal },
                    pUnkForRelease: ManuallyDrop::new(None),
                });
            }

            if fmt.cfFormat == self.shared.cf_contents && tymed_istream {
                if self.shared.is_ui_thread() {
                    return Err(DV_E_FORMATETC.into());
                }
                let entries = self.shared.wait_plan()?;
                let Ok(index) = usize::try_from(fmt.lindex) else {
                    return Err(DV_E_FORMATETC.into());
                };
                let Some(entry) = entries.get(index) else {
                    return Err(DV_E_FORMATETC.into());
                };
                if entry.is_dir {
                    return Err(DV_E_FORMATETC.into());
                }
                let stream: IStream = RemoteFileStream {
                    reader: Mutex::new(StreamReader::new(
                        self.shared.source.clone(),
                        entry.source.clone(),
                        entry.follow_links,
                        entry.size,
                    )),
                    expected_size: entry.size,
                    shared: self.shared.clone(),
                }
                .into();
                return Ok(STGMEDIUM {
                    tymed: TYMED_ISTREAM.0 as u32,
                    u: STGMEDIUM_0 {
                        pstm: ManuallyDrop::new(Some(stream)),
                    },
                    pUnkForRelease: ManuallyDrop::new(None),
                });
            }

            if fmt.cfFormat == self.shared.cf_preferred_effect && tymed_hglobal {
                let bytes = DROPEFFECT_COPY.0.to_ne_bytes();
                let hglobal = alloc_hglobal_bytes(&bytes)?;
                return Ok(STGMEDIUM {
                    tymed: TYMED_HGLOBAL.0 as u32,
                    u: STGMEDIUM_0 { hGlobal: hglobal },
                    pUnkForRelease: ManuallyDrop::new(None),
                });
            }

            if tymed_hglobal {
                let stored = self.shared.stored.lock().unwrap();
                if let Some((_, bytes)) = stored.iter().find(|(cf, _)| *cf == fmt.cfFormat) {
                    let hglobal = alloc_hglobal_bytes(bytes)?;
                    return Ok(STGMEDIUM {
                        tymed: TYMED_HGLOBAL.0 as u32,
                        u: STGMEDIUM_0 { hGlobal: hglobal },
                        pUnkForRelease: ManuallyDrop::new(None),
                    });
                }
            }

            Err(DV_E_FORMATETC.into())
        }

        fn GetDataHere(&self, _: *const FORMATETC, _: *mut STGMEDIUM) -> WinResult<()> {
            Err(E_NOTIMPL.into())
        }

        fn QueryGetData(&self, pformatetc: *const FORMATETC) -> windows::core::HRESULT {
            if pformatetc.is_null() {
                return E_INVALIDARG;
            }
            let fmt = unsafe { *pformatetc };
            if self.supports_query(&fmt) {
                S_OK
            } else {
                DV_E_FORMATETC
            }
        }

        fn GetCanonicalFormatEtc(
            &self,
            _: *const FORMATETC,
            _: *mut FORMATETC,
        ) -> windows::core::HRESULT {
            E_NOTIMPL
        }

        fn SetData(
            &self,
            pformatetc: *const FORMATETC,
            pmedium: *const STGMEDIUM,
            frelease: BOOL,
        ) -> WinResult<()> {
            if pformatetc.is_null() || pmedium.is_null() {
                return Err(E_INVALIDARG.into());
            }
            let fmt = unsafe { *pformatetc };
            let medium = unsafe { &*pmedium };
            if medium.tymed != TYMED_HGLOBAL.0 as u32 {
                return Err(E_NOTIMPL.into());
            }
            let handle = unsafe { medium.u.hGlobal };
            let size = unsafe { GlobalSize(handle) };
            let mut bytes = vec![0u8; size];
            if size > 0 {
                let ptr = unsafe { GlobalLock(handle) };
                if ptr.is_null() {
                    return Err(E_FAIL.into());
                }
                unsafe {
                    std::ptr::copy_nonoverlapping(ptr.cast::<u8>(), bytes.as_mut_ptr(), size);
                    let _ = GlobalUnlock(handle);
                }
            }
            {
                let mut stored = self.shared.stored.lock().unwrap();
                if let Some(existing) = stored.iter_mut().find(|(cf, _)| *cf == fmt.cfFormat) {
                    existing.1 = bytes;
                } else {
                    stored.push((fmt.cfFormat, bytes));
                }
            }
            if frelease.as_bool() {
                unsafe { ReleaseStgMedium(pmedium.cast_mut()) };
            }
            Ok(())
        }

        fn EnumFormatEtc(&self, dwdirection: u32) -> WinResult<IEnumFORMATETC> {
            if dwdirection != DATADIR_GET.0 as u32 {
                return Err(E_NOTIMPL.into());
            }
            let formats = [
                VirtualFilesDataObject::format_etc(
                    self.shared.cf_descriptor,
                    TYMED_HGLOBAL.0 as u32,
                    -1,
                ),
                VirtualFilesDataObject::format_etc(
                    self.shared.cf_contents,
                    TYMED_ISTREAM.0 as u32,
                    -1,
                ),
                VirtualFilesDataObject::format_etc(
                    self.shared.cf_preferred_effect,
                    TYMED_HGLOBAL.0 as u32,
                    -1,
                ),
            ];
            unsafe { SHCreateStdEnumFmtEtc(&formats) }
        }

        fn DAdvise(
            &self,
            _: *const FORMATETC,
            _: u32,
            _: Ref<'_, IAdviseSink>,
        ) -> WinResult<u32> {
            Err(OLE_E_ADVISENOTSUPPORTED.into())
        }

        fn DUnadvise(&self, _: u32) -> WinResult<()> {
            Err(OLE_E_ADVISENOTSUPPORTED.into())
        }

        fn EnumDAdvise(&self) -> WinResult<IEnumSTATDATA> {
            Err(OLE_E_ADVISENOTSUPPORTED.into())
        }
    }

    #[allow(non_snake_case)]
    impl IDataObjectAsyncCapability_Impl for VirtualFilesDataObject_Impl {
        fn SetAsyncMode(&self, fdoopasync: BOOL) -> WinResult<()> {
            self.shared
                .async_mode
                .store(fdoopasync.as_bool(), Ordering::Release);
            Ok(())
        }

        fn GetAsyncMode(&self) -> WinResult<BOOL> {
            Ok(BOOL::from(self.shared.async_mode.load(Ordering::Acquire)))
        }

        fn StartOperation(
            &self,
            _: Ref<'_, windows::Win32::System::Com::IBindCtx>,
        ) -> WinResult<()> {
            self.shared.in_operation.store(true, Ordering::Release);
            Ok(())
        }

        fn InOperation(&self) -> WinResult<BOOL> {
            Ok(BOOL::from(
                self.shared.in_operation.load(Ordering::Acquire),
            ))
        }

        fn EndOperation(
            &self,
            _: windows::core::HRESULT,
            _: Ref<'_, windows::Win32::System::Com::IBindCtx>,
            _: u32,
        ) -> WinResult<()> {
            self.shared.in_operation.store(false, Ordering::Release);
            Ok(())
        }
    }

    #[implement(IStream)]
    struct RemoteFileStream {
        reader: Mutex<StreamReader>,
        expected_size: Option<u64>,
        shared: Arc<Shared>,
    }

    #[allow(non_snake_case)]
    impl ISequentialStream_Impl for RemoteFileStream_Impl {
        fn Read(&self, pv: *mut c_void, cb: u32, pcbread: *mut u32) -> windows::core::HRESULT {
            if self.shared.is_ui_thread() {
                return E_FAIL;
            }
            if cb > 0 && pv.is_null() {
                return E_INVALIDARG;
            }
            let out = if cb == 0 {
                &mut []
            } else {
                unsafe { std::slice::from_raw_parts_mut(pv.cast::<u8>(), cb as usize) }
            };
            let mut reader = self.reader.lock().unwrap();
            let mut total = 0usize;
            while total < out.len() {
                match reader.read_into(&mut out[total..]) {
                    Ok(0) => break,
                    Ok(read) => total += read,
                    Err(error) => {
                        self.shared.report_error(&format!("{error:#}"));
                        if !pcbread.is_null() {
                            unsafe { *pcbread = 0 };
                        }
                        return E_FAIL;
                    }
                }
            }
            if !pcbread.is_null() {
                unsafe { *pcbread = total as u32 };
            }
            if total == out.len() { S_OK } else { S_FALSE }
        }

        fn Write(&self, _: *const c_void, _: u32, _: *mut u32) -> windows::core::HRESULT {
            E_NOTIMPL
        }
    }

    #[allow(non_snake_case)]
    impl IStream_Impl for RemoteFileStream_Impl {
        fn Seek(
            &self,
            dlibmove: i64,
            dworigin: STREAM_SEEK,
            plibnewposition: *mut u64,
        ) -> WinResult<()> {
            let reader = self.reader.lock().unwrap();
            let pos = reader.position();
            let supported = (dworigin == STREAM_SEEK_CUR && dlibmove == 0)
                || (dworigin == STREAM_SEEK_SET && dlibmove >= 0 && dlibmove as u64 == pos)
                || (dworigin == STREAM_SEEK_END && dlibmove == 0 && self.expected_size == Some(pos));
            if !supported {
                return Err(STG_E_INVALIDFUNCTION.into());
            }
            if !plibnewposition.is_null() {
                unsafe { *plibnewposition = pos };
            }
            Ok(())
        }

        fn SetSize(&self, _: u64) -> WinResult<()> {
            Err(E_NOTIMPL.into())
        }

        fn CopyTo(
            &self,
            _: Ref<'_, IStream>,
            _: u64,
            _: *mut u64,
            _: *mut u64,
        ) -> WinResult<()> {
            Err(E_NOTIMPL.into())
        }

        fn Commit(&self, _: &STGC) -> WinResult<()> {
            Ok(())
        }

        fn Revert(&self) -> WinResult<()> {
            Err(E_NOTIMPL.into())
        }

        fn LockRegion(&self, _: u64, _: u64, _: &LOCKTYPE) -> WinResult<()> {
            Err(E_NOTIMPL.into())
        }

        fn UnlockRegion(&self, _: u64, _: u64, _: u32) -> WinResult<()> {
            Err(E_NOTIMPL.into())
        }

        fn Stat(&self, pstatstg: *mut STATSTG, _: &STATFLAG) -> WinResult<()> {
            if pstatstg.is_null() {
                return Err(E_INVALIDARG.into());
            }
            let stat = STATSTG {
                pwcsName: windows::core::PWSTR::null(),
                r#type: STGTY_STREAM.0 as u32,
                cbSize: self.expected_size.unwrap_or(0),
                ..Default::default()
            };
            unsafe { *pstatstg = stat };
            Ok(())
        }

        fn Clone(&self) -> WinResult<IStream> {
            Err(E_NOTIMPL.into())
        }
    }

    #[cfg(test)]
    pub(super) fn utf16_array_to_string(buf: &[u16]) -> String {
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..len])
    }

    #[cfg(test)]
    pub(super) fn pcwstr_cf_descriptor() -> windows::core::PCWSTR {
        CFSTR_FILEDESCRIPTORW
    }

    #[cfg(test)]
    pub(super) fn pcwstr_cf_contents() -> windows::core::PCWSTR {
        CFSTR_FILECONTENTS
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use anyhow::Context as _;
    use gpui::px;

    use super::*;

    struct FakeRemoteSource {
        plan: Vec<VirtualEntry>,
        files: HashMap<String, Vec<u8>>,
        reported_size_override: Option<u64>,
        truncate_after_first_read: bool,
    }

    impl FakeRemoteSource {
        fn new(plan: Vec<VirtualEntry>, files: HashMap<String, Vec<u8>>) -> Self {
            Self {
                plan,
                files,
                reported_size_override: None,
                truncate_after_first_read: false,
            }
        }
    }

    impl RemoteSource for FakeRemoteSource {
        fn plan(&self) -> anyhow::Result<Vec<VirtualEntry>> {
            Ok(self.plan.clone())
        }

        fn read_chunk(
            &self,
            source: &str,
            _follow_links: bool,
            offset: u64,
            length: u64,
        ) -> anyhow::Result<(Vec<u8>, u64)> {
            let data = self
                .files
                .get(source)
                .with_context(|| format!("missing fake file {source}"))?;
            let total = self.reported_size_override.unwrap_or(data.len() as u64);
            if self.truncate_after_first_read && offset > 0 {
                return Ok((Vec::new(), total));
            }
            let start = (offset as usize).min(data.len());
            let end = start.saturating_add(length as usize).min(data.len());
            Ok((data[start..end].to_vec(), total))
        }
    }

    #[test]
    fn drag_threshold_uses_euclidean_distance() {
        let origin = Point::new(px(10.0), px(20.0));
        assert!(!drag_threshold_exceeded(origin, origin));
        assert!(!drag_threshold_exceeded(origin, Point::new(px(14.0), px(24.0))));
        assert!(drag_threshold_exceeded(origin, Point::new(px(16.0), px(20.0))));
        assert!(drag_threshold_exceeded(origin, Point::new(px(14.5), px(24.5))));
    }

    #[test]
    fn stream_reader_reads_across_chunks_and_odd_slices() {
        let payload: Vec<u8> = (0..=u8::MAX).cycle().take(1_500_000).collect();
        let source = Arc::new(FakeRemoteSource::new(
            vec![],
            HashMap::from([("big.bin".into(), payload.clone())]),
        ));
        let mut reader = StreamReader::new(source, "big.bin".into(), true, None);
        let mut out = Vec::new();
        let mut scratch = [0u8; 7919];
        loop {
            let n = reader.read_into(&mut scratch).unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&scratch[..n]);
            assert_eq!(reader.position(), out.len() as u64);
        }
        assert_eq!(out, payload);
        assert_eq!(reader.read_into(&mut scratch).unwrap(), 0);
    }

    #[test]
    fn stream_reader_rejects_size_changes_and_early_eof() {
        let source = Arc::new(FakeRemoteSource::new(
            vec![],
            HashMap::from([("file.bin".into(), vec![1u8; 10])]),
        ));
        let mut reader = StreamReader::new(source, "file.bin".into(), true, Some(99));
        let mut buf = [0u8; 4];
        let err = reader.read_into(&mut buf).unwrap_err();
        assert!(err.to_string().contains("changed size"));

        let mut eof_source = FakeRemoteSource::new(
            vec![],
            HashMap::from([("short.bin".into(), vec![1u8; 10])]),
        );
        eof_source.reported_size_override = Some(20);
        eof_source.truncate_after_first_read = true;
        let mut reader = StreamReader::new(Arc::new(eof_source), "short.bin".into(), false, None);
        let mut large = vec![0u8; 10];
        assert_eq!(reader.read_into(&mut large).unwrap(), 10);
        let err = reader.read_into(&mut large).unwrap_err();
        assert!(err.to_string().contains("ended early"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_local_and_virtual_drag_objects_expose_expected_formats() {
        use std::ffi::c_void;
        use uuid::Uuid;
        use ::windows::Win32::Foundation::{E_FAIL, S_OK};
        use ::windows::Win32::Storage::FileSystem::{
            FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL,
        };
        use ::windows::Win32::System::Com::{
            DVASPECT_CONTENT, FORMATETC, IDataObject, TYMED_HGLOBAL, TYMED_ISTREAM,
        };
        use ::windows::Win32::System::DataExchange::RegisterClipboardFormatW;
        use ::windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
        use ::windows::Win32::System::Ole::{CF_HDROP, OleInitialize, ReleaseStgMedium};
        use ::windows::Win32::UI::Shell::{FILEDESCRIPTORW, FILEGROUPDESCRIPTORW};

        let _ = unsafe { OleInitialize(None) };

        // (a) local_data_object answers QueryGetData(CF_HDROP) with S_OK
        let temp_dir = std::env::temp_dir().join(format!("waku-drag-local-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let temp_file = temp_dir.join("hello.txt");
        std::fs::write(&temp_file, b"hello").unwrap();
        let local_obj = self::windows::local_data_object(&temp_file).unwrap();
        let hdrop_fmt = FORMATETC {
            cfFormat: CF_HDROP.0,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0 as u32,
        };
        assert_eq!(unsafe { local_obj.QueryGetData(&hdrop_fmt) }, S_OK);
        let _ = std::fs::remove_dir_all(&temp_dir);

        // (b) VirtualFilesDataObject for a folder with 1 dir + 2 files
        let plan = vec![
            VirtualEntry {
                descriptor_path: "bundle".into(),
                source: "bundle".into(),
                size: None,
                is_dir: true,
                follow_links: false,
            },
            VirtualEntry {
                descriptor_path: r"bundle\a.txt".into(),
                source: "bundle/a.txt".into(),
                size: Some(5),
                is_dir: false,
                follow_links: false,
            },
            VirtualEntry {
                descriptor_path: r"bundle\sub\b.bin".into(),
                source: "bundle/sub/b.bin".into(),
                size: Some(2048),
                is_dir: false,
                follow_links: false,
            },
        ];
        let b_bytes: Vec<u8> = (0..=u8::MAX).cycle().take(2048).collect();
        let source = Arc::new(FakeRemoteSource::new(
            plan,
            HashMap::from([
                ("bundle/a.txt".into(), b"hello".to_vec()),
                ("bundle/sub/b.bin".into(), b_bytes.clone()),
            ]),
        ));
        let (tx, _rx) = smol::channel::bounded(4);
        let obj: IDataObject = self::windows::VirtualFilesDataObject::new(source, tx, None).into();

        let cf_desc =
            unsafe { RegisterClipboardFormatW(self::windows::pcwstr_cf_descriptor()) } as u16;
        let cf_cont =
            unsafe { RegisterClipboardFormatW(self::windows::pcwstr_cf_contents()) } as u16;
        let desc_fmt = FORMATETC {
            cfFormat: cf_desc,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0 as u32,
        };
        let mut medium = unsafe { obj.GetData(&desc_fmt) }.unwrap();
        let ptr = unsafe { GlobalLock(medium.u.hGlobal) }.cast::<u8>();
        assert!(!ptr.is_null());
        unsafe {
            let count = ptr.cast::<u32>().read_unaligned();
            assert_eq!(count, 3);
            let items = ptr
                .add(std::mem::offset_of!(FILEGROUPDESCRIPTORW, fgd))
                .cast::<FILEDESCRIPTORW>();
            let item0 = items.add(0).read_unaligned();
            assert_eq!(
                self::windows::utf16_array_to_string(&{ item0.cFileName }),
                "bundle"
            );
            assert_eq!({ item0.dwFileAttributes }, FILE_ATTRIBUTE_DIRECTORY.0);

            let item1 = items.add(1).read_unaligned();
            assert_eq!(
                self::windows::utf16_array_to_string(&{ item1.cFileName }),
                r"bundle\a.txt"
            );
            assert_eq!({ item1.dwFileAttributes }, FILE_ATTRIBUTE_NORMAL.0);
            assert_eq!({ item1.nFileSizeLow }, 5);

            let item2 = items.add(2).read_unaligned();
            assert_eq!(
                self::windows::utf16_array_to_string(&{ item2.cFileName }),
                r"bundle\sub\b.bin"
            );
            assert_eq!({ item2.nFileSizeLow }, 2048);

            let _ = GlobalUnlock(medium.u.hGlobal);
            ReleaseStgMedium(&mut medium);
        }

        let cont_fmt = FORMATETC {
            cfFormat: cf_cont,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex: 2,
            tymed: TYMED_ISTREAM.0 as u32,
        };
        let mut stream_medium = unsafe { obj.GetData(&cont_fmt) }.unwrap();
        let stream = unsafe { (*stream_medium.u.pstm).clone() }.unwrap();
        let mut read_back = Vec::new();
        let mut buf = [0u8; 512];
        loop {
            let mut n = 0u32;
            let hr = unsafe {
                stream.Read(
                    buf.as_mut_ptr().cast::<c_void>(),
                    buf.len() as u32,
                    Some(&mut n),
                )
            };
            read_back.extend_from_slice(&buf[..n as usize]);
            if hr != S_OK {
                break;
            }
        }
        assert_eq!(read_back, b_bytes);
        unsafe { ReleaseStgMedium(&mut stream_medium) };

        // (c) too-long path yields E_FAIL and one error on channel
        let long_source = Arc::new(FakeRemoteSource::new(
            vec![VirtualEntry {
                descriptor_path: "a".repeat(260),
                source: "long".into(),
                size: Some(1),
                is_dir: false,
                follow_links: true,
            }],
            HashMap::new(),
        ));
        let (tx_err, rx_err) = smol::channel::bounded(4);
        let err_obj: IDataObject =
            self::windows::VirtualFilesDataObject::new(long_source, tx_err, None).into();
        let res = unsafe { err_obj.GetData(&desc_fmt) };
        let Err(err) = res else {
            panic!("expected GetData to fail for an oversized descriptor path");
        };
        assert_eq!(err.code(), E_FAIL);
        assert!(rx_err.try_recv().is_ok());
        assert!(rx_err.try_recv().is_err());
    }
}
