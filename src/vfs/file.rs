use crate::{
    config::de_duration_sec,
    login::ManagedOnedrive,
    vfs::{Error, Result, UpdateEvent},
};
use bytes::Bytes;
use lru_cache::LruCache;
use onedrive_api::{
    resource::{DriveItem, DriveItemField},
    ItemId, ItemLocation, OneDrive, Tag,
};
use reqwest::{header, StatusCode};
use serde::Deserialize;
use sharded_slab::Slab;
use std::{
    convert::TryFrom as _,
    io::{self, SeekFrom},
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex as SyncMutex, Weak,
    },
    time::{Duration, Instant, SystemTime},
};
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    sync::{mpsc, watch, Mutex, MutexGuard},
    time,
};

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    disk_cache: DiskCacheConfig,
    download: DownloadConfig,
    upload: UploadConfig,
}

#[derive(Debug, Deserialize, Clone)]
struct DownloadConfig {
    max_retry: usize,
    #[serde(deserialize_with = "de_duration_sec")]
    retry_delay: Duration,
    stream_buffer_chunks: usize,
}

#[derive(Debug, Deserialize, Clone)]
struct DiskCacheConfig {
    enable: bool,
    #[serde(default = "default_disk_cache_dir")]
    path: PathBuf,
    max_cached_file_size: u64,
    max_files: usize,
    max_total_size: u64,
}

#[derive(Debug, Deserialize, Clone)]
struct UploadConfig {
    max_size: u64,
    #[serde(deserialize_with = "de_duration_sec")]
    flush_delay: Duration,
    #[serde(deserialize_with = "de_duration_sec")]
    retry_delay: Duration,
}

fn default_disk_cache_dir() -> PathBuf {
    std::env::temp_dir().join("onedrive_fuse-cache")
}

pub struct FilePool {
    handles: Slab<File>,
    disk_cache: Option<DiskCache>,
    event_tx: mpsc::Sender<UpdateEvent>,
    config: Config,
}

#[derive(Debug, Clone)]
pub struct UpdatedFileAttr {
    pub item_id: ItemId,
    pub size: u64,
    pub mtime: SystemTime,
    /// `None` indicates that the CTag is currently unknown.
    pub c_tag: Option<Tag>,
}

impl FilePool {
    pub const SYNC_SELECT_FIELDS: &'static [DriveItemField] = &[DriveItemField::c_tag];

    pub fn new(event_tx: mpsc::Sender<UpdateEvent>, config: Config) -> anyhow::Result<Self> {
        Ok(Self {
            handles: Slab::new(),
            disk_cache: if config.disk_cache.enable {
                Some(DiskCache::new(config.clone())?)
            } else {
                None
            },
            event_tx,
            config,
        })
    }

    fn key_to_fh(key: usize) -> u64 {
        u64::try_from(key).unwrap()
    }

    fn fh_to_key(fh: u64) -> usize {
        usize::try_from(fh).unwrap()
    }

    // Fetch file size, CTag and download URL.
    async fn fetch_meta(item_id: &ItemId, onedrive: &OneDrive) -> Result<(u64, Tag, String)> {
        // `download_url` is available without `$select`.
        let item = onedrive.get_item(ItemLocation::from_id(item_id)).await?;
        let file_size = item.size.unwrap() as u64;
        let tag = item.c_tag.unwrap();
        let download_url = item.download_url.unwrap();
        Ok((file_size, tag, download_url))
    }

    async fn open_inner(
        &self,
        item_id: &ItemId,
        write_mode: bool,
        onedrive: &OneDrive,
        client: &reqwest::Client,
    ) -> Result<File> {
        let (file_size, download_url) = if let Some(cache) = &self.disk_cache {
            if let Some(state) = cache.get(item_id) {
                log::debug!("File already cached: {:?}", item_id);
                return Ok(File::Cached(state));
            }

            let (file_size, c_tag, download_url) = Self::fetch_meta(item_id, onedrive).await?;
            if let Some(state) =
                cache.try_alloc_and_fetch(item_id, file_size, c_tag, &download_url, client)?
            {
                log::debug!("Caching file {:?}, url: {}", item_id, download_url);
                return Ok(File::Cached(state));
            } else if write_mode {
                return Err(Error::FileTooLarge);
            }

            (file_size, download_url)
        } else if write_mode {
            return Err(Error::WriteWithoutCache);
        } else {
            let (file_size, _, download_url) = Self::fetch_meta(item_id, onedrive).await?;
            (file_size, download_url)
        };

        log::debug!("Streaming file {:?}, url: {}", item_id, download_url);
        let state = FileStreamState::fetch(
            file_size,
            download_url,
            client.clone(),
            self.config.download.clone(),
        );
        Ok(File::Streaming {
            file_size,
            state: Arc::new(Mutex::new(state)),
        })
    }

    pub async fn open(
        &self,
        item_id: &ItemId,
        write_mode: bool,
        onedrive: &OneDrive,
        client: &reqwest::Client,
    ) -> Result<u64> {
        let file = self
            .open_inner(item_id, write_mode, onedrive, client)
            .await?;
        let key = self.handles.insert(file).expect("Pool is full");
        Ok(Self::key_to_fh(key))
    }

    pub async fn close(&self, fh: u64) -> Result<()> {
        if self.handles.remove(Self::fh_to_key(fh)) {
            Ok(())
        } else {
            Err(Error::InvalidHandle(fh))
        }
    }

    pub async fn read(&self, fh: u64, offset: u64, size: usize) -> Result<impl AsRef<[u8]>> {
        let file = self
            .handles
            .get(Self::fh_to_key(fh))
            .ok_or(Error::InvalidHandle(fh))?
            .clone();
        match file {
            File::Streaming { file_size, state } => {
                state.lock().await.read(file_size, offset, size).await
            }
            File::Cached(state) => FileCache::read(&state, offset, size).await,
        }
    }

    /// Write to cached file. Returns item id and file size after the write.
    pub async fn write(
        &self,
        fh: u64,
        offset: u64,
        data: &[u8],
        onedrive: ManagedOnedrive,
    ) -> Result<UpdatedFileAttr> {
        let file = self
            .handles
            .get(Self::fh_to_key(fh))
            .ok_or(Error::InvalidHandle(fh))?
            .clone();
        match file {
            File::Streaming { .. } => panic!("Cannot stream in write mode"),
            File::Cached(state) => {
                FileCache::write(
                    &state,
                    offset,
                    data,
                    self.event_tx.clone(),
                    onedrive,
                    self.config.upload.clone(),
                )
                .await
            }
        }
    }

    pub async fn sync_items(&self, items: &[DriveItem]) {
        if let Some(cache) = &self.disk_cache {
            cache.sync_items(items).await;
        }
    }
}

#[derive(Debug, Clone)]
enum File {
    Streaming {
        file_size: u64,
        state: Arc<Mutex<FileStreamState>>,
    },
    Cached(Arc<FileCache>),
}

#[derive(Debug)]
struct FileStreamState {
    current_pos: u64,
    buffer: Option<Bytes>,
    rx: mpsc::Receiver<Bytes>,
}

impl FileStreamState {
    fn fetch(
        file_size: u64,
        download_url: String,
        client: reqwest::Client,
        config: DownloadConfig,
    ) -> Self {
        let (tx, rx) = mpsc::channel(config.stream_buffer_chunks);
        tokio::spawn(download_thread(file_size, download_url, tx, client, config));
        Self {
            current_pos: 0,
            buffer: None,
            rx,
        }
    }

    /// `offset` and `size` must be already clamped.
    async fn read(&mut self, file_size: u64, offset: u64, size: usize) -> Result<Bytes> {
        if offset != self.current_pos {
            return Err(Error::NonsequentialRead {
                current_pos: self.current_pos,
                try_offset: offset,
            });
        }

        let mut ret_buf = Vec::with_capacity(size);
        loop {
            let chunk = match self.buffer.take() {
                Some(chunk) => chunk,
                None => match self.rx.recv().await {
                    Some(chunk) => chunk,
                    None => break,
                },
            };

            let buf_rest_len = ret_buf.capacity() - ret_buf.len();
            if buf_rest_len < chunk.len() {
                self.buffer = Some(chunk.slice(buf_rest_len..));
                ret_buf.extend_from_slice(&chunk[..buf_rest_len]);
                break;
            } else {
                ret_buf.extend_from_slice(&chunk);
                if ret_buf.len() == ret_buf.capacity() {
                    break;
                }
            }
        }

        self.current_pos += ret_buf.len() as u64;

        if ret_buf.len() == size {
            Ok(ret_buf.into())
        } else {
            Err(Error::UnexpectedEndOfDownload {
                current_pos: self.current_pos,
                file_size,
            })
        }
    }
}

async fn download_thread(
    file_size: u64,
    download_url: String,
    tx: mpsc::Sender<Bytes>,
    client: reqwest::Client,
    config: DownloadConfig,
) {
    let mut pos = 0u64;

    log::debug!("Start downloading ({} bytes)", file_size);

    while pos < file_size {
        let mut tries = 0;
        let mut resp = loop {
            let ret: anyhow::Result<_> = client
                .get(&download_url)
                .header(header::RANGE, format!("bytes={}-", pos))
                .send()
                .await
                .map_err(|err| err.into())
                .and_then(|resp| {
                    if resp.status() != StatusCode::PARTIAL_CONTENT {
                        anyhow::bail!("Not Partial Content response: {}", resp.status());
                    }
                    Ok(resp)
                });
            match ret {
                Ok(resp) => break resp,
                Err(err) => {
                    tries += 1;
                    log::error!(
                        "Error downloading file (try {}/{}): {}",
                        tries,
                        config.max_retry,
                        err,
                    );
                    if config.max_retry < tries {
                        return;
                    }
                    tokio::time::sleep(config.retry_delay).await;
                }
            }
        };

        while let Some(chunk) = resp.chunk().await.ok().flatten() {
            pos += chunk.len() as u64;
            assert!(pos <= file_size);
            if tx.send(chunk).await.is_err() {
                return;
            }
        }
    }

    assert_eq!(pos, file_size);
    log::debug!("Download finished ({} bytes)", file_size);
}

#[derive(Debug)]
struct DiskCache {
    dir: PathBuf,
    total_size: Arc<AtomicU64>,
    cache: SyncMutex<LruCache<ItemId, Arc<FileCache>>>,
    config: Config,
}

impl DiskCache {
    fn new(config: Config) -> io::Result<Self> {
        let disk_config = &config.disk_cache;
        assert!(disk_config.enable);
        assert!(disk_config.max_cached_file_size <= disk_config.max_total_size);

        let dir = disk_config.path.clone();
        std::fs::create_dir_all(&dir)?;
        log::debug!("Disk file cache enabled at: {}", dir.display());
        Ok(Self {
            dir,
            total_size: Arc::new(0.into()),
            cache: SyncMutex::new(LruCache::new(disk_config.max_files)),
            config,
        })
    }

    fn get(&self, item_id: &ItemId) -> Option<Arc<FileCache>> {
        self.cache.lock().unwrap().get_mut(item_id).cloned()
    }

    fn try_alloc_and_fetch(
        &self,
        item_id: &ItemId,
        file_size: u64,
        c_tag: Tag,
        download_url: &str,
        client: &reqwest::Client,
    ) -> io::Result<Option<Arc<FileCache>>> {
        if self.config.disk_cache.max_cached_file_size < file_size {
            return Ok(None);
        }

        let mut cache = self.cache.lock().unwrap();
        if let Some(state) = cache.get_mut(&item_id) {
            return Ok(Some(state.clone()));
        }

        // Drop LRU until we have enough space.
        while self.config.disk_cache.max_cached_file_size
            < self.total_size.load(Ordering::Relaxed) + file_size
        {
            if cache.remove_lru().is_none() {
                // Cache is already empty.
                return Ok(None);
            }
        }

        let cache_file = tempfile::tempfile_in(&self.dir)?;
        cache_file.set_len(file_size)?;

        // The channel size doesn't really matter, since it's just for synchronization
        // between downloading and writing.
        let (chunk_tx, chunk_rx) = mpsc::channel(64);
        let state = FileCache::new(
            item_id.clone(),
            file_size,
            c_tag,
            chunk_rx,
            cache_file.into(),
            &self.total_size,
        );
        cache.insert(item_id.clone(), state.clone());
        tokio::spawn(download_thread(
            file_size,
            download_url.to_owned(),
            chunk_tx,
            client.clone(),
            self.config.download.clone(),
        ));
        Ok(Some(state))
    }

    async fn sync_items(&self, items: &[DriveItem]) {
        let mut outdated = Vec::new();
        {
            let mut cache = self.cache.lock().unwrap();
            for item in items {
                if item.folder.is_some() {
                    continue;
                }
                let id = item.id.clone().expect("Missing id");
                let c_tag = item.c_tag.clone().expect("Missing c_tag");
                let file = match cache.get_mut(&id) {
                    Some(file) => file,
                    None => continue,
                };
                let old_c_tag = file.c_tag.lock().unwrap();
                if *old_c_tag == c_tag {
                    log::debug!("Cached file {:?} is still up-to-date", *old_c_tag);
                } else {
                    log::debug!(
                        "Cached file {:?} is outdated, ctag: {:?} -> {:?}",
                        file.item_id,
                        *old_c_tag,
                        c_tag,
                    );
                    drop(old_c_tag);
                    outdated.push(cache.remove(&id).unwrap());
                }
            }
        }
        for file in outdated {
            file.state.lock().await.status = FileCacheStatus::Invalidated;
        }
    }
}

#[derive(Debug)]
struct FileCache {
    state: Mutex<FileCacheState>,
    item_id: ItemId,
    c_tag: SyncMutex<Tag>,
    cache_total_size: Weak<AtomicU64>,
}

#[derive(Debug)]
struct FileCacheState {
    status: FileCacheStatus,
    file_size: u64,
    available_size: watch::Receiver<u64>,
    cache_file: tokio::fs::File,
}

#[derive(Debug)]
enum FileCacheStatus {
    /// File is downloading.
    Downloading,
    /// File is downloaded or created, and is synchronized with remote side.
    Available,
    /// File is downloaded or created, and is uploading or waiting for uploading.
    Dirty(Instant),
    /// File is changed in remote side, local cache is invalidated.
    Invalidated,
}

impl FileCache {
    fn new(
        item_id: ItemId,
        file_size: u64,
        c_tag: Tag,
        chunk_rx: mpsc::Receiver<Bytes>,
        cache_file: tokio::fs::File,
        cache_total_size: &Arc<AtomicU64>,
    ) -> Arc<Self> {
        let (pos_tx, pos_rx) = watch::channel(0);
        let this = Arc::new(Self {
            state: Mutex::new(FileCacheState {
                status: FileCacheStatus::Downloading,
                file_size,
                available_size: pos_rx,
                cache_file,
            }),
            item_id,
            c_tag: SyncMutex::new(c_tag),
            cache_total_size: Arc::downgrade(&cache_total_size),
        });
        cache_total_size.fetch_add(file_size, Ordering::Relaxed);

        tokio::spawn(Self::write_to_cache_thread(
            Arc::downgrade(&this),
            chunk_rx,
            pos_tx,
        ));
        this
    }

    async fn write_to_cache_thread(
        this: Weak<FileCache>,
        mut chunk_rx: mpsc::Receiver<Bytes>,
        pos_tx: watch::Sender<u64>,
    ) {
        let mut pos = 0u64;
        while let Some(chunk) = chunk_rx.recv().await {
            let file = match this.upgrade() {
                Some(file) => file,
                None => return,
            };
            let mut guard = file.state.lock().await;
            guard.cache_file.seek(SeekFrom::Start(pos)).await.unwrap();
            guard.cache_file.write_all(&chunk).await.unwrap();

            pos += chunk.len() as u64;
            log::trace!(
                "Write {} bytes to cache, current pos: {}, total size: {}",
                chunk.len(),
                pos,
                guard.file_size,
            );

            // We are holding `state`.
            pos_tx.send(pos).unwrap();

            if pos == guard.file_size {
                log::debug!("Cache available ({} bytes)", guard.file_size);
                assert!(matches!(guard.status, FileCacheStatus::Downloading));
                guard.status = FileCacheStatus::Available;
                return;
            }
        }
    }

    /// Wait until download completion, or at least `end` bytes in total are available.
    async fn wait_bytes_available(
        this: &Arc<Self>,
        end: u64,
    ) -> Result<MutexGuard<'_, FileCacheState>> {
        let mut rx = {
            let guard = this.state.lock().await;
            match guard.status {
                FileCacheStatus::Available | FileCacheStatus::Dirty(_) => return Ok(guard),
                FileCacheStatus::Invalidated => return Err(Error::Invalidated),
                FileCacheStatus::Downloading => {}
            }
            if end <= *guard.available_size.borrow() {
                return Ok(guard);
            }

            guard.available_size.clone()
        };

        while rx.changed().await.is_ok() && *rx.borrow() < end {}

        let guard = this.state.lock().await;
        match guard.status {
            FileCacheStatus::Available | FileCacheStatus::Dirty(_) => return Ok(guard),
            FileCacheStatus::Invalidated => return Err(Error::Invalidated),
            FileCacheStatus::Downloading => {
                let available = *rx.borrow();
                if available < end {
                    return Err(Error::UnexpectedEndOfDownload {
                        current_pos: available,
                        file_size: guard.file_size,
                    });
                }
                Ok(guard)
            }
        }
    }

    async fn read(this: &Arc<Self>, offset: u64, size: usize) -> Result<Bytes> {
        let end = {
            let guard = this.state.lock().await;
            let file_size = guard.file_size;
            if file_size <= offset || size == 0 {
                return Ok(Bytes::new());
            }
            file_size.min(offset + size as u64)
        };

        let mut guard = Self::wait_bytes_available(this, end).await?;

        let mut buf = vec![0u8; (end - offset) as usize];
        guard
            .cache_file
            .seek(SeekFrom::Start(offset))
            .await
            .unwrap();
        guard.cache_file.read_exact(&mut buf).await.unwrap();
        Ok(buf.into())
    }

    async fn write(
        this: &Arc<Self>,
        offset: u64,
        data: &[u8],
        event_tx: mpsc::Sender<UpdateEvent>,
        onedrive: ManagedOnedrive,
        config: UploadConfig,
    ) -> Result<UpdatedFileAttr> {
        let file_size = this.state.lock().await.file_size;
        if config.max_size < offset + data.len() as u64 {
            return Err(Error::FileTooLarge);
        }

        let mut guard = Self::wait_bytes_available(this, file_size).await?;
        let mtime = Instant::now();
        let mtime_sys = SystemTime::now();
        match guard.status {
            FileCacheStatus::Downloading | FileCacheStatus::Invalidated => unreachable!(),
            FileCacheStatus::Dirty(_) | FileCacheStatus::Available => {
                guard.status = FileCacheStatus::Dirty(mtime);
                tokio::spawn(Self::upload_thread(
                    this.clone(),
                    onedrive,
                    mtime,
                    event_tx.clone(),
                    config,
                ));
            }
        }

        guard
            .cache_file
            .seek(SeekFrom::Start(offset))
            .await
            .unwrap();
        guard.cache_file.write_all(data).await.unwrap();

        let new_size = guard.file_size.max(offset + data.len() as u64);
        if guard.file_size < new_size {
            if let Some(total) = this.cache_total_size.upgrade() {
                total.fetch_add(new_size - guard.file_size, Ordering::Relaxed);
            }
        }
        guard.file_size = new_size;
        Ok(UpdatedFileAttr {
            item_id: this.item_id.clone(),
            size: new_size,
            mtime: mtime_sys,
            c_tag: None,
        })
    }

    async fn upload_thread(
        this: Arc<Self>,
        onedrive: ManagedOnedrive,
        lock_mtime: Instant,
        event_tx: mpsc::Sender<UpdateEvent>,
        config: UploadConfig,
    ) {
        time::sleep(config.flush_delay).await;

        loop {
            // Check not changed since last lock.
            let data = {
                let mut guard = this.state.lock().await;
                match guard.status {
                    FileCacheStatus::Dirty(mtime) if mtime == lock_mtime => {
                        let mut buf = vec![0u8; guard.file_size as usize];
                        guard.cache_file.seek(SeekFrom::Start(0)).await.unwrap();
                        guard.cache_file.read_exact(&mut buf).await.unwrap();
                        buf
                    }
                    _ => return,
                }
            };
            let file_len = data.len();

            // Do upload.
            log::info!("Uploading {:?} ({} B)", this.item_id, file_len);
            let item = match onedrive
                .get()
                .await
                .upload_small(ItemLocation::from_id(&this.item_id), data)
                .await
            {
                Ok(item) => item,
                Err(err) => {
                    log::error!(
                        "Failed to upload {:?} ({} B): {}",
                        this.item_id,
                        file_len,
                        err,
                    );
                    // Retry
                    time::sleep(config.retry_delay).await;
                    continue;
                }
            };

            log::info!("Uploaded {:?} ({} B)", this.item_id, file_len);

            let attr = super::InodeAttr::parse_item(&item).expect("Invalid attrs");
            assert_eq!(item.id.as_ref(), Some(&this.item_id));
            assert_eq!(attr.size, file_len as u64);
            let c_tag = item.c_tag.expect("Missing c_tag");
            *this.c_tag.lock().unwrap() = c_tag.clone();
            let _ = event_tx
                .send(UpdateEvent::UpdateFile(UpdatedFileAttr {
                    item_id: this.item_id.clone(),
                    size: attr.size,
                    mtime: attr.mtime,
                    c_tag: Some(c_tag),
                }))
                .await;
            break;
        }

        let mut guard = this.state.lock().await;
        match guard.status {
            FileCacheStatus::Dirty(mtime) if mtime == lock_mtime => {
                guard.status = FileCacheStatus::Available;
            }
            _ => {}
        }
    }
}

impl Drop for FileCache {
    fn drop(&mut self) {
        if let Some(arc) = self.cache_total_size.upgrade() {
            arc.fetch_sub(self.state.get_mut().file_size, Ordering::Relaxed);
        }
    }
}
