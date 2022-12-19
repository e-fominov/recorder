use crate::model::{SourceConfig, StreamProfile};
use crate::StreamCompressionParams;
use anyhow::{anyhow, Result};
use chrono::{Datelike, Timelike, Utc};
use gstreamer::glib::Value;
use gstreamer::prelude::*;
use gstreamer::{element_error, Pipeline};
use std::collections::BTreeMap;
use std::fmt::Write as _; // import without risk of name clashing
use std::path::{Path, PathBuf};
use tokio::sync::broadcast::Sender;
use tracing::*;

static VIDEO_TIMESTAMP_FORMAT: &str = "%y%m%dT%H%M%S%.3f";

#[derive(Debug, Clone)]
pub(crate) struct Frame {
    pub _data: Vec<u8>,
}

pub(crate) fn start_pipeline(
    name: &str,
    params: &SourceConfig,
    profile: &StreamProfile,
    root: &Path,
    pipes: &BTreeMap<String, (Sender<Frame>, StreamCompressionParams)>,
) -> Result<Pipeline> {
    info!("Initializing GStreamer for {name}");
    gstreamer::init().unwrap();

    let mut pipeline_text = if params.source == "test" {
        "videotestsrc".into()
    } else if params.source.starts_with("http:") {
        format!("souphttpsrc location=\"{}\" is-live=true do-timestamp=true ! multipartdemux ! image/jpeg ! jpegdec", params.source)
    } else {
        panic!("Unable to handle source : {}", params.source);
    };
    let _ = write!(
        pipeline_text,
        " ! videorate ! video/x-raw,framerate={},width={},height={}",
        profile.framerate, profile.width, profile.height
    );
    if let Some(clock) = &profile.clock {
        let format = clock.format.replace("${name}", name);
        let _ = write!(
            pipeline_text,
            " ! clockoverlay halignment={} valignment={} \
        shaded-background=true font-desc=\"{}\" time-format=\"{}\"",
            clock.halignment, clock.valignment, clock.font, format
        );
    }
    pipeline_text += " ! tee name=decoded";
    for (encoding, prm) in &profile.encode {
        pipeline_text += " decoded. ! queue";
        let _ = write!(
            pipeline_text,
            " ! videoscale ! video/x-raw,width={},height={}",
            &prm.width, &prm.height
        );
        let _ = write!(
            pipeline_text,
            " ! x264enc bframes=0 bitrate={} speed-preset={} tune={} key-int-max={}",
            prm.bitrate, prm.preset, prm.tune, prm.keyint
        );
        if prm.record {
            let _ = write!(pipeline_text, " ! tee name=x264_{}", encoding);
            let _ = write!(
                pipeline_text,
                " ! splitmuxsink max-size-time={}000000000 muxer-factory=mp4mux name=rec_{}",
                prm.segment_seconds, encoding
            );
            let _ = write!(
                pipeline_text,
                " x264_{}. ! h264parse ! video/x-h264,stream-format=byte-stream ! appsink name=app_{}",
                encoding, encoding
            );
        } else {
            let _ = write!(pipeline_text, " ! video/x-h264,stream-format=byte-stream");
            let _ = write!(pipeline_text, " ! appsink name=app_{}", encoding);
        }
    }
    if params.show {
        pipeline_text += " decoded. ! autovideosink";
    }

    info!("Creating pipeline for {}", name);
    let pipeline = gstreamer::parse_launch(&pipeline_text)
        .map_err(|e| anyhow!("Unable to launch pipeline for {name} because: {e}"))?;

    let pipeline = pipeline
        .dynamic_cast::<Pipeline>()
        .map_err(|_e| anyhow!("Unable to cast pipeline element into Pipeline struct"))?;
    for (encoding, encparams) in &profile.encode {
        if encparams.record {
            let rec_name = format!("rec_{encoding}");
            let recorder = pipeline.by_name(&rec_name).unwrap();
            let encoding_name = encoding.clone();
            let src_name = name.to_string();
            let root = PathBuf::from(root);
            let segment_seconds = encparams.segment_seconds;
            recorder.connect(
                "format-location-full",
                false,
                move |params: &[Value]| -> Option<Value> {
                    let ts = Utc::now();
                    let _splitmux = params[0].get::<gstreamer::Element>().unwrap();
                    let _fragment_id = params[1].get::<u32>().unwrap();
                    let first_sample = params[2].get::<gstreamer::Sample>().unwrap();
                    let pts = first_sample.buffer().unwrap().pts();
                    let dir = root
                        .join(&src_name)
                        .join(&encoding_name)
                        .join(&ts.year().to_string())
                        .join(&format!("{:02}", ts.month()))
                        .join(&format!("{:02}", ts.day()))
                        .join(&format!("{:02}", ts.hour()));
                    if !dir.exists() {
                        std::fs::create_dir_all(&dir).unwrap_or_else(|e| {
                            panic!(
                                "Unable to create directory: {}. Because: {e}",
                                dir.display()
                            )
                        });
                    }
                    let path = dir.join(format!(
                        "{}_{}.mp4",
                        ts.format(VIDEO_TIMESTAMP_FORMAT),
                        segment_seconds * 1000
                    ));
                    info!("New file path = {}, pts = {}", path.display(), pts.unwrap());
                    Some(Value::from(&path.display().to_string()))
                },
            );
        }

        let app_name = format!("app_{encoding}");
        let appsink = pipeline
            .by_name(&app_name)
            .expect("App sink should be created earlier");
        let appsink = appsink
            .dynamic_cast::<gstreamer_app::AppSink>()
            .map_err(|_| anyhow!("Unable to cast app sink"))?;

        let tx = pipes
            .get(encoding)
            .expect("Pipes should be created with same config file")
            .0
            .clone();
        appsink.set_callbacks(
            gstreamer_app::AppSinkCallbacks::builder()
                .new_sample(move |appsink| {
                    let sample = appsink
                        .pull_sample()
                        .map_err(|_| gstreamer::FlowError::Eos)?;
                    let buffer = sample.buffer().ok_or_else(|| {
                        element_error!(
                            appsink,
                            gstreamer::ResourceError::Failed,
                            ("Failed to get buffer from appsink")
                        );

                        gstreamer::FlowError::Error
                    })?;
                    let map = buffer.map_readable().map_err(|_| {
                        element_error!(
                            appsink,
                            gstreamer::ResourceError::Failed,
                            ("Failed to map buffer readable")
                        );

                        gstreamer::FlowError::Error
                    })?;
                    let samples = map.as_slice();
                    // ignore the send errors due to possible stream overflow
                    // if something really bad happens, pipeline should be explicitly sopped instead
                    let _ignore = tx.send(Frame {
                        _data: samples.to_vec(),
                    });

                    Ok(gstreamer::FlowSuccess::Ok)
                })
                .build(),
        );
    }

    Ok(pipeline)
}
