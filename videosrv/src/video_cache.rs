use crate::Timestamp;
use anyhow::{anyhow, Result};
use chrono::{Datelike, Duration, NaiveDateTime, Timelike, Utc};
use itertools::Itertools;
use serde::Serialize;
use std::collections::HashMap;
use std::fmt::Formatter;
use std::fs;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{interval, MissedTickBehavior};
use tracing::{debug, error, info, trace, warn};
use walkdir::WalkDir;

static TIMESTAMP_KEY_FORMAT: &str = "%y%m%dT%H%M%S%.3f";
static VIDEO_TIMESTAMP_FORMAT: &str = "%y%m%dT%H%M%S%.3f";
static VIDEO_JITTER_CORRECTION_MS: i64 = 5000;

pub(super) struct VideoCacheInterface {
    tx_req: mpsc::Sender<VideoCacheReq>,
    tx_req_recordings_list: mpsc::Sender<RecordingsListReq>,
}

#[derive(Clone)]
pub(super) struct VideoCacheParams {
    pub(super) video_root: PathBuf,
    pub(super) cache_root: PathBuf,
    pub(super) keep_alive: Duration,
}

#[derive(Debug)]
pub(super) struct HlsPlaylist {
    pub(super) files: Vec<HlsPlaylistItem>,
}
#[derive(Debug)]
pub(super) struct HlsPlaylistItem {
    pub(super) relpath: String,
    pub(super) _timestamp: Timestamp,
    pub(super) duration: Duration,
}
pub(super) fn start_video_cache(params: VideoCacheParams) -> Result<VideoCacheInterface> {
    info!("Starting video cache");
    let (tx_req, rx_req) = mpsc::channel::<_>(16);
    let (tx_req_recordings_list, rx_req_recordings_list) = mpsc::channel::<_>(16);

    tokio::spawn(async move {
        let res = video_cache_proc(params, rx_req, rx_req_recordings_list).await;
        if let Err(e) = res {
            error!("Video cache start error: {e}");
        } else {
            info!("Video cache stopped");
        }
    });
    Ok(VideoCacheInterface {
        tx_req,
        tx_req_recordings_list,
    })
}

pub(super) struct VideoCacheReq {
    pub(super) timestamp_min: Timestamp,
    pub(super) timestamp_max: Timestamp,
    pub(super) file_name: String,
    pub(super) container: String,
    pub(super) resolution: String,
    pub(super) callback: oneshot::Sender<Result<PathBuf>>,
}

#[derive(Debug)]
struct RecordingsListReq {
    timestamp_min: Timestamp,
    timestamp_max: Timestamp,
    container: String,
    resolution: String,
    callback: oneshot::Sender<Result<Vec<RecordingListItem>>>,
}
#[derive(Debug, Serialize)]
pub(super) struct RecordingListItem {
    pub(super) timestamp_min: Timestamp,
    pub(super) timestamp_max: Timestamp,
}

struct VideoCache {
    params: VideoCacheParams,
    items: HashMap<PathBuf, VideoCacheItem>,
}

struct VideoCacheItem {
    path: PathBuf,
    timestamp: Timestamp,
}

async fn video_cache_proc(
    params: VideoCacheParams,
    mut rx_req: mpsc::Receiver<VideoCacheReq>,
    mut rx_req_recordings_list: mpsc::Receiver<RecordingsListReq>,
) -> Result<()> {
    let mut cache_cleanup_interval = interval(std::time::Duration::from_secs(60));
    cache_cleanup_interval.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let cache = VideoCache::new(params.clone())?;
    let cache = Arc::new(Mutex::new(cache));

    let cache = cache.clone();
    loop {
        tokio::select! {
            Some(req) = rx_req.recv() => {
                if let Err(e) = handle_request(cache.clone(), req).await {
                    error!("Error processing request: {e}");
                }
            }
            Some(req) = rx_req_recordings_list.recv() => {
                let res = handle_request_recordings_list(cache.clone(), &req).await;
                if let Err(e) = req.callback.send(res){
                    error!("Error sending response: {e:?}");
                }
            }
            _ = cache_cleanup_interval.tick() => {
                if let Err(e) = cache_cleanup(cache.clone()).await {
                     error!("Error on timer: {e}");
                }
            }
        }
    }
    //Ok(())
}
async fn handle_request(cache: Arc<Mutex<VideoCache>>, req: VideoCacheReq) -> Result<()> {
    let filename = req
        .file_name
        .rfind('/')
        .map(|ix| req.file_name.as_str()[ix + 1..].to_string())
        .unwrap_or_else(|| req.file_name.clone());

    trace!("Video cache request, path : {}", req.file_name);
    let (cache_dir, item_path, _begin, _end) = {
        let mut cache = cache.lock().expect("Unable to lock video cache mutex");

        let key = cache.unpack(
            &req.container,
            &req.resolution,
            req.timestamp_min,
            req.timestamp_max,
        );
        let cache_dir = PathBuf::from(&cache.params.cache_root).join(&key);
        let item_path = cache_dir.join(&filename);
        // cached item exists, just return its path
        if let Some(item) = cache.items.get_mut(&item_path) {
            item.timestamp = Utc::now();
            let _ignore = req.callback.send(Ok(item_path));

            write_timestamp_file(&cache_dir)?;
            return Ok(());
        }
        (cache_dir, item_path, req.timestamp_min, req.timestamp_max)
    };
    if !cache_dir.exists() {
        info!("Creating directory : {}", cache_dir.display());
        fs::create_dir_all(&cache_dir)?;
    }
    create_file(
        cache.clone(),
        &req.file_name,
        &item_path,
        req.timestamp_min,
        req.timestamp_max,
        &req.container,
        &req.resolution,
    )
    .await?;

    let mut cache = cache.lock().expect("Unable to lock video cache mutex");
    cache.items.insert(
        PathBuf::from(&req.file_name),
        VideoCacheItem {
            path: item_path.clone(),
            timestamp: Utc::now(),
        },
    );
    write_timestamp_file(&cache_dir)?;
    let _ignore = req.callback.send(Ok(item_path));
    Ok(())
}

async fn handle_request_recordings_list(
    cache: Arc<Mutex<VideoCache>>,
    req: &RecordingsListReq,
) -> Result<Vec<RecordingListItem>> {
    trace!("Recordings list request, container : {}, resolution: {}, timestamp_min: {}, timestamp_max: {}", 
        req.container, req.resolution, req.timestamp_min.to_rfc3339(), req.timestamp_max.to_rfc3339());

    let res = {
        let cache = cache
            .lock()
            .map_err(|e| anyhow!("Unable to lock video cache mutex. Because {e}"))?;

        let list = cache.find_recordings(
            &req.container,
            &req.resolution,
            req.timestamp_min,
            req.timestamp_max,
        );
        info!("Found {} files", list.len());

        let mut res = Vec::<RecordingListItem>::new();
        for item in list {
            let item_timestamp_max = item.1 + item.2;
            let updated = match res.last_mut() {
                Some(i) => {
                    let overlap = (i.timestamp_max
                        + Duration::milliseconds(VIDEO_JITTER_CORRECTION_MS))
                    .min(item_timestamp_max)
                        > (i.timestamp_min - Duration::milliseconds(VIDEO_JITTER_CORRECTION_MS))
                            .max(item.1);
                    if overlap {
                        i.timestamp_max = i.timestamp_max.max(item_timestamp_max);
                    }
                    overlap
                }
                None => false,
            };
            if !updated {
                res.push(RecordingListItem {
                    timestamp_min: item.1,
                    timestamp_max: item_timestamp_max,
                });
            }
        }
        res
    };

    Ok(res)
}

async fn create_file(
    cache: Arc<Mutex<VideoCache>>,
    request: &str,
    out_path: &PathBuf,
    begin: Timestamp,
    end: Timestamp,
    container: &str,
    resolution: &str,
) -> Result<()> {
    let file_name = out_path
        .file_name()
        .map(|s| s.to_string_lossy())
        .expect("Should have file name");
    match &*file_name {
        "playlist.m3u8" => create_playlist(cache, out_path, begin, end, container, resolution),
        _ if file_name.ends_with(".ts") => {
            create_ts(cache, container, resolution, request, out_path).await
        }
        _ => Err(anyhow!("Unable to create {} file", out_path.display())),
    }
}

fn create_playlist(
    cache: Arc<Mutex<VideoCache>>,
    out_path: &PathBuf,
    begin: Timestamp,
    end: Timestamp,
    container: &str,
    resolution: &str,
) -> Result<()> {
    let cache = cache
        .lock()
        .map_err(|e| anyhow!("Unable to lock cache: {e}"))?;
    let playlist: HlsPlaylist = cache
        .find_recordings(container, resolution, begin, end)
        .into_iter()
        .into();
    info!("Writing playlist: {}", out_path.display());
    let mut file = File::create(&out_path)?;
    playlist.write(&mut file)?;
    Ok(())
}

fn parse_relpath(path: &str) -> Option<(&str, &str, Timestamp, Duration)> {
    let mut splits = path.split('/');
    let camera = splits.next();
    let resolution = splits.next();
    let _year = splits.next();
    let _month = splits.next();
    let _day = splits.next();
    let _hour = splits.next();
    let filename = splits.next();
    if let Some((camera, resolution, filename)) = camera.and_then(|camera| {
        filename.and_then(|filename| resolution.map(|resolution| (camera, resolution, filename)))
    }) {
        let mut fsplits = filename.split('_');
        if let Some((ts, dur)) = fsplits.next().and_then(|sts| {
            NaiveDateTime::parse_from_str(sts, VIDEO_TIMESTAMP_FORMAT)
                .ok()
                //parse_timestamp(sts)
                .and_then(|ts| {
                    fsplits.next().and_then(|ext| {
                        ext.split('.')
                            .next()
                            .and_then(|dur| dur.parse::<i64>().ok().map(|dur| (ts, dur)))
                    })
                })
        }) {
            return Some((
                camera,
                resolution,
                Timestamp::from_utc(ts, Utc),
                Duration::milliseconds(dur),
            ));
        }
    }
    None
}
async fn create_ts(
    cache: Arc<Mutex<VideoCache>>,
    container: &str,
    resolution: &str,
    request: &str,
    out_path: &Path,
) -> Result<()> {
    let dir = out_path.parent().expect("Should have cache directory");

    debug!("Create ts request: {request} -> {}", out_path.display());
    let mut splits = request.split('_');
    let filename = splits.next();
    let duration = splits.next().and_then(|s| s.parse::<i64>().ok());
    let start_time = splits
        .next()
        .and_then(|s| s.split('.').next().and_then(|s| s.parse::<i64>().ok()));
    let ts = filename
        .and_then(|filestem| NaiveDateTime::parse_from_str(filestem, VIDEO_TIMESTAMP_FORMAT).ok());
    // debug!("{filename:?}, {start_time:?}, {ts:?}");
    if let (Some(filestem), Some(duration), Some(start_ms), Some(ts)) =
        (filename, duration, start_time, ts)
    {
        // debug!("filestem={}", filestem);
        // some known file found
        let mp4_name = format!("{}_{}.mp4", filestem, duration);
        let mp4_path = dir.join(&mp4_name);
        if !mp4_path.exists() {
            let path = {
                let cache = cache.lock().expect("Unable to lock cache");
                cache
                    .params
                    .video_root
                    .join(container)
                    .join(resolution)
                    .join(&ts.year().to_string())
                    .join(&format!("{:02}", ts.month()))
                    .join(&format!("{:02}", ts.day()))
                    .join(&format!("{:02}", ts.hour()))
                    .join(mp4_name)
            };
            // debug!("path={}", path.display());
            // have a local file
            if path.exists() {
                debug!(
                    "Copying existing file {} into {}",
                    path.display(),
                    mp4_path.display()
                );
                tokio::fs::copy(path, &mp4_path).await?;
            }
        }
        // todo, handle non-zero start point
        mp4_to_ts(&mp4_path, out_path, Duration::seconds(0), start_ms).await?;
        debug!("Removing temporary file {}", mp4_path.display());
        fs::remove_file(&mp4_path)?;
        Ok(())
    } else {
        Err(anyhow!("Unable to parse file name {}", request))
    }
}
async fn cache_cleanup(cache: Arc<Mutex<VideoCache>>) -> Result<()> {
    let mut cache = cache.lock().expect("Unable to lock video cache mutex");
    let mut remove = Vec::new();
    let now = Utc::now();
    let keep_alive = cache.params.keep_alive;
    for (key, item) in &mut cache.items {
        let elapsed = now - item.timestamp;
        if elapsed >= keep_alive {
            if item.path.exists() {
                if item.path.is_dir() {
                    info!("Removing cache dir: {}", item.path.display());
                    fs::remove_dir_all(&item.path)?;
                } else {
                    info!("Removing cache file: {}", item.path.display());
                    fs::remove_file(&item.path)?;
                }
            }
            remove.push(key.clone());
        }
    }
    for key in remove {
        cache.items.remove(&key);
    }
    Ok(())
}

pub(super) async fn mp4_to_ts(
    input: &Path,
    output: &Path,
    time_offset: Duration,
    start_ms: i64,
) -> Result<()> {
    // ffmpeg requires /2 to the delay

    // for small starting timestamps, its better to use muxdelay/preload because they don't add 1.4
    // to the beginning
    // for larger offsets, outout_ts_offset is better
    let input = PathBuf::from(input);
    let output = PathBuf::from(output);
    let status = tokio::task::spawn_blocking(move || {
        let status = if start_ms > 10000 {
            let start_ts = (start_ms as f32 / 1000.0 - 1.4).to_string();
            Command::new("ffmpeg")
                .arg("-hide_banner")
                .arg("-loglevel")
                .arg("info")
                .arg("-i")
                .arg(input.display().to_string())
                .arg("-ss")
                .arg(format!(
                    "{:02}:{:02}:{:02}",
                    time_offset.num_hours(),
                    time_offset.num_minutes() % 60,
                    time_offset.num_seconds() % 60
                ))
                .arg("-y")
                .arg("-codec")
                .arg("copy")
                .arg("-output_ts_offset")
                .arg(&start_ts)
                .arg(output.display().to_string())
                .status()?
        } else {
            let delay = (start_ms as f32 / 2000.0).to_string();
            Command::new("ffmpeg")
                .arg("-hide_banner")
                .arg("-loglevel")
                .arg("info")
                .arg("-i")
                .arg(input.display().to_string())
                .arg("-ss")
                .arg(format!(
                    "{:02}:{:02}:{:02}",
                    time_offset.num_hours(),
                    time_offset.num_minutes() % 60,
                    time_offset.num_seconds() % 60
                ))
                .arg("-y")
                .arg("-codec")
                .arg("copy")
                .arg("-muxdelay")
                .arg(&delay)
                .arg("-muxpreload")
                .arg(&delay)
                .arg(output.display().to_string())
                .status()?
        };
        Result::<ExitStatus>::Ok(status)
    })
    .await??;

    if !status.success() {
        return Err(anyhow!("Error executing ffmpeg"));
    }
    Ok(())
}

impl VideoCacheInterface {
    pub(super) async fn request(
        &self,
        file_path: String,
        container: String,
        resolution: String,
        timestamp_min: Timestamp,
        timestamp_max: Timestamp,
    ) -> Result<PathBuf> {
        let (callback, rx) = oneshot::channel::<Result<PathBuf>>();
        let req = VideoCacheReq {
            timestamp_min,
            timestamp_max,
            file_name: file_path,
            container,
            resolution,
            callback,
        };
        self.tx_req.send(req).await?;
        let res = rx.await??;
        Ok(res)
    }

    /// Send a request to Video Cache querying the list of available recorded time ranges
    /// Returns: list of time ranges available for query recordings
    pub(super) async fn request_recordings_list(
        &self,
        container: String,
        resolution: String,
        timestamp_min: Timestamp,
        timestamp_max: Timestamp,
    ) -> Result<Vec<RecordingListItem>> {
        let (callback, rx) = oneshot::channel::<Result<Vec<RecordingListItem>>>();
        let req = RecordingsListReq {
            timestamp_min,
            timestamp_max,
            container,
            resolution,
            callback,
        };
        self.tx_req_recordings_list.send(req).await?;
        let res = rx.await??;
        Ok(res)
    }
}

impl std::fmt::Debug for VideoCacheReq {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "TimeRange: {:?}-{:?}, file: {}",
            self.timestamp_min, self.timestamp_max, self.file_name
        )
    }
}

impl VideoCache {
    fn new(params: VideoCacheParams) -> Result<Self> {
        let mut cache = Self {
            params,
            items: Default::default(),
        };
        cache.initialize_cache()?;
        Ok(cache)
    }
    fn initialize_cache(&mut self) -> Result<()> {
        let now = Utc::now();
        for item in fs::read_dir(&self.params.cache_root)?.flatten() {
            if item.path().is_dir() {
                let timestamp = read_timestamp_file(&item.path());
                let remove = if let Some(timestamp) = timestamp {
                    let elapsed = now - timestamp;
                    if elapsed < self.params.keep_alive {
                        let item = VideoCacheItem {
                            path: item.path().clone(),
                            timestamp,
                        };
                        info!("Found existing cache item: {}", item.path.display());
                        self.items.insert(item.path.clone(), item);
                        false
                    } else {
                        true
                    }
                } else {
                    // do not remove anything that does not have timestamp in our internal format
                    // this will keep us safe from misconfiguration errors
                    warn!(
                        "Found invalid cache item: {}. Please remove it manually",
                        item.path().display()
                    );
                    false
                };
                if remove {
                    info!("Removing old cache: {}", item.path().display());
                    fs::remove_dir_all(item.path())?;
                }
            }
        }
        Ok(())
    }
    fn unpack(
        &self,
        container: &str,
        resolution: &str,
        timestamp_min: Timestamp,
        timestamp_max: Timestamp,
    ) -> String {
        format!(
            "{container}:{resolution}:{}:{}",
            timestamp_min.format(TIMESTAMP_KEY_FORMAT),
            timestamp_max.format(TIMESTAMP_KEY_FORMAT)
        )
    }

    /// searches video root for video files matching parameters
    fn find_recordings(
        &self,
        container: &str,
        resolution: &str,
        timestamp_min: Timestamp,
        timestamp_max: Timestamp,
    ) -> Vec<(PathBuf, Timestamp, Duration)> {
        // where
        let storage_root = self.params.video_root.join(container).join(resolution);
        let files: Vec<(PathBuf, Timestamp, Duration)> = WalkDir::new(storage_root)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path().is_file()
                    && e.path()
                        .extension()
                        .map_or(false, |ex| ex.to_string_lossy() == "mp4")
            })
            .filter_map(|e| {
                e.path()
                    .strip_prefix(&self.params.video_root)
                    .ok()
                    .map(|x| x.display().to_string())
                    .and_then(|x| {
                        parse_relpath(&x).map(|(_cam, _res, timestamp, duration)| {
                            (e.path().into(), timestamp, duration)
                        })
                    })
            })
            .filter(|(_path, ts, _dur)| ts >= &timestamp_min && ts <= &timestamp_max)
            .sorted_by(|a, b| a.1.cmp(&b.1)) // by timestamp
            .collect();
        files
    }
}
fn _video_file_is_good(path: &Path) -> bool {
    debug!("checking video file {}", path.display());
    let res = Command::new("ffmpeg")
        .arg("-i")
        .arg(path.display().to_string())
        .arg("-c")
        .arg("copy")
        .arg("-f")
        .arg("null")
        .arg("/dev/null")
        .status()
        .map_or(false, |res| res.success());
    debug!("is_good: {res}");
    res
}
fn write_timestamp_file(dir: &Path) -> Result<()> {
    let file_path = dir.join("timestamp");
    let mut f = File::create(file_path)?;
    f.write_all(Utc::now().to_rfc3339().as_bytes())?;
    Ok(())
}
fn read_timestamp_file(dir: &Path) -> Option<Timestamp> {
    let file_path = dir.join("timestamp");
    let mut f = File::open(file_path).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    let s = String::from_utf8_lossy(&buf);
    Timestamp::from_str(&s).ok()
}

impl<'a, I> From<I> for HlsPlaylist
where
    I: Iterator<Item = (PathBuf, Timestamp, Duration)>,
{
    fn from(iter: I) -> Self {
        Self {
            files: iter
                .map(|(path, ts, dur)| HlsPlaylistItem {
                    relpath: path
                        .file_name()
                        .map(|x| x.to_string_lossy().to_string())
                        .unwrap_or_else(|| "".to_string()),
                    _timestamp: ts,
                    duration: dur,
                })
                .collect(),
        }
    }
}

impl HlsPlaylist {
    pub fn write<W: Write>(&self, writer: &mut W) -> Result<()> {
        writer.write_all(b"#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-MEDIA-SEQUENCE:0\n")?;
        let mut start_ms = 0;
        for item in &self.files {
            let millis = item.duration.num_milliseconds();
            let txt = format!(
                "#EXTINF:{:.4}\n{}_{}.ts\n",
                millis as f64 / 1000.0,
                item.relpath.replace(".mp4", ""),
                start_ms
            );
            start_ms += millis;
            writer.write_all(txt.as_bytes())?;
        }
        writer.write_all(b"#EXT-X-ENDLIST")?;
        Ok(())
    }
}
