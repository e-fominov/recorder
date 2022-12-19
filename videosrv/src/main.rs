extern crate core;

mod error;
mod gs;
mod model;
mod video_cache;

use crate::error::ErrorCode;
use crate::gs::{start_pipeline, Frame};
use crate::model::{Config, StreamCompressionParams};
use crate::video_cache::{start_video_cache, VideoCacheParams};
use actix_cors::Cors;
use actix_files::NamedFile;
use actix_web::http::StatusCode;
use actix_web::{get, middleware, web, App, HttpResponse, HttpServer, Responder};
use anyhow::{anyhow, Result};
use chrono::{DateTime, NaiveDateTime, Utc};
use gstreamer::prelude::ElementExt;
use serde::Deserialize;
use serde_json::json;
use std::collections::BTreeMap;
use std::fs::File;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use structopt::StructOpt;
use tokio::sync::broadcast::error::TryRecvError;
use tokio::sync::broadcast::{channel, Sender};
use tracing::*;
use video_cache::VideoCacheInterface;

const FRAME_CHANNEL_CAPACITY: usize = 16;
static CONNECTION_LOST_THRESHOLD_MS: i64 = 30000;

type Timestamp = DateTime<Utc>;

#[derive(StructOpt)]
#[structopt(name = "videosrv", about = "Video server")]
struct Options {
    #[structopt(long, default_value = "./video.yaml")]
    pub config: PathBuf,
    #[structopt(long, default_value = "8080")]
    pub port: u16,
    #[structopt(long, default_value = "2")]
    pub actix_workers: usize,
}

#[get("/")]
async fn index() -> impl Responder {
    NamedFile::open_async("./static/index.html").await.unwrap()
}

// Retreives information about what sources & profiles there are
#[get("/info")]
async fn get_info(data: web::Data<Config>) -> HttpResponse {
    let sources: Vec<String> = data.sources.keys().cloned().collect();
    let profiles: Vec<String> = data
        .profiles
        .values()
        .flat_map(|val| val.encode.keys().cloned())
        .collect();

    HttpResponse::Ok().json(json!({ "sources": sources, "profiles": profiles }))
}

fn parse_date_range(range: &str) -> Result<(Timestamp, Timestamp)> {
    let dates: Vec<NaiveDateTime> = range
        .split('-')
        .filter_map(|s| NaiveDateTime::parse_from_str(s, "%y%m%dT%H%M%S").ok())
        .collect();
    if dates.len() == 2 {
        Ok((
            Timestamp::from_utc(dates[0], Utc),
            Timestamp::from_utc(dates[1], Utc),
        ))
    } else {
        Err(anyhow!("Unable to parse time range from {range}"))
    }
}

/// Creates new WebSocket session and connects it to a video pipe
#[get("/rec/{source}/{encoding}/{timerange}/{path}")]
async fn recording(
    data: web::Data<VideoData>,
    params: web::Path<(String, String, String, String)>,
) -> Result<NamedFile, ErrorCode> {
    let (source, encoding, timerange, path) = params.into_inner();
    info!("Requested recording for {source} with encoding = {encoding}, time range; {timerange}, path: {path}");
    let (dbegin, dend) = parse_date_range(&timerange)?;
    if dend <= dbegin {
        return Err(ierror!(
            "Invalid time range, begin date should be prior to end"
        ));
    }
    let vc = &data.vc;
    let cached_path = vc.request(path, source, encoding, dbegin, dend).await?;
    trace!("Reading cached file {}", cached_path.display());
    let cached_file_stream = NamedFile::open_async(cached_path).await?;
    Ok(cached_file_stream)
}

#[derive(Deserialize, Debug)]
struct GetRecordingsListQuery {
    timestamp_min: Timestamp,
    timestamp_max: Timestamp,
    source: String,
    resolution: String,
}

#[get("/recording_list")]
async fn get_recording_list(
    query: web::Query<GetRecordingsListQuery>,
    data: web::Data<VideoData>,
) -> Result<HttpResponse, ErrorCode> {
    info!(
        "get_recordings_list, source: {}, resolution: {}, timestamp_min: {}, timestamp_max: {}",
        query.source,
        query.resolution,
        query.timestamp_min.to_rfc3339(),
        query.timestamp_max.to_rfc3339()
    );
    let vc = &data.vc;
    let list = vc
        .request_recordings_list(
            query.source.clone(),
            query.resolution.clone(),
            query.timestamp_min,
            query.timestamp_max,
        )
        .await?;

    Ok(HttpResponse::build(StatusCode::OK).json(list))
}
/// Application shared data
struct VideoData {
    vc: Arc<VideoCacheInterface>,
}

#[actix_web::main]
async fn main() -> Result<()> {
    dotenv::dotenv().ok();

    env_logger::init_from_env(env_logger::Env::new().default_filter_or("info"));
    let opt: Options = Options::from_args();
    info!("Loading config from {}", opt.config.display());
    let cfg: Config = serde_yaml::from_reader(File::open(&opt.config)?)?;

    let mut pipes: BTreeMap<String, BTreeMap<String, (Sender<Frame>, StreamCompressionParams)>> =
        BTreeMap::new();

    info!("Starting {} source pipelines", cfg.sources.len());
    for (name, params) in &cfg.sources {
        let name = name.to_string();
        let params = params.clone();
        let root = cfg.storage.root.clone();
        let profile = params
            .profile
            .clone()
            .unwrap_or_else(|| "default".to_string());
        let profile = cfg
            .profiles
            .get(&profile)
            .cloned()
            .unwrap_or_else(|| panic!("Unable to find profile {profile}"));
        let src_pipes: BTreeMap<String, (Sender<Frame>, StreamCompressionParams)> = profile
            .encode
            .iter()
            .map(|(encoding, params)| {
                let (tx, _) = channel::<Frame>(FRAME_CHANNEL_CAPACITY);
                (encoding.clone(), (tx, params.clone()))
            })
            .collect();

        if let Some((tx, _params)) = src_pipes.values().next() {
            let mut rx = tx.subscribe();
            let mut pipeline = start_pipeline(&name, &params, &profile, &root, &src_pipes)?;
            pipes.insert(name.to_string(), src_pipes.clone());

            let source_name = name.to_string();
            // create a watchdog thread that starts and reconnects the pipeline if no
            // frames are received within the specified interval
            let _thread = std::thread::spawn(move || {
                info!("Starting pipeline source = [{source_name}]");
                pipeline
                    .set_state(gstreamer::State::Playing)
                    .expect("Unable to set the pipeline to the `Playing` state");
                let mut last_frame_time = Utc::now();
                loop {
                    match rx.try_recv() {
                        Ok(_) => {
                            last_frame_time = Utc::now();
                            continue;
                        }
                        Err(e) => match e {
                            TryRecvError::Empty | TryRecvError::Lagged(_) => {
                                let now = Utc::now();
                                let elapsed = now - last_frame_time;
                                if elapsed.num_milliseconds() >= CONNECTION_LOST_THRESHOLD_MS {
                                    warn!(
                                        "{source_name} connection lost by timeout.
    elapsed time [{elapsed}] is greater than  timeout [{CONNECTION_LOST_THRESHOLD_MS}].
    Stopping pipeline"
                                    );
                                    pipeline
                                        .set_state(gstreamer::State::Null)
                                        .expect("Unable to set the pipeline to the `Null` state");
                                    warn!("{} connection lost. Re-creating pipeline", source_name);
                                    pipeline = start_pipeline(
                                        &source_name,
                                        &params,
                                        &profile,
                                        &root,
                                        &src_pipes,
                                    )?;
                                    warn!("{} connection lost. Starting pipeline", source_name);
                                    pipeline.set_state(gstreamer::State::Playing).expect(
                                        "Unable to set the pipeline to the `Playing` state",
                                    );
                                    last_frame_time = Utc::now();
                                } else {
                                    std::thread::sleep(Duration::from_millis(100));
                                }
                            }
                            TryRecvError::Closed => {
                                info!("Channel closed for {}", source_name);
                                break;
                            }
                        },
                    }
                }
                Result::<()>::Ok(())
            });
        } else {
            error!("No pipes defined for {}", name)
        }
    }

    let _pipes = Arc::new(Mutex::new(pipes));

    let vc = start_video_cache(VideoCacheParams {
        video_root: cfg.storage.root.clone(),
        cache_root: cfg.storage.cache.clone(),
        keep_alive: chrono::Duration::milliseconds(60000),
    })?;
    let vc = Arc::new(vc);
    let data = web::Data::new(VideoData { vc });
    let cfg = web::Data::new(cfg);

    // Here we're creating the server and binding it to port 8080.
    HttpServer::new(move || {
        let cors = Cors::default()
            .allow_any_header()
            .allow_any_origin()
            .allow_any_method()
            .send_wildcard();

        App::new()
            .app_data(data.clone())
            .app_data(cfg.clone())
            .wrap(cors)
            .service(index)
            .service(get_info)
            .service(recording)
            .service(get_recording_list)
            .wrap(middleware::Logger::default())
    })
    .workers(opt.actix_workers)
    .bind(("0.0.0.0", opt.port))?
    .run()
    .await?;

    info!("Closing pipeline");
    Ok(())
}
