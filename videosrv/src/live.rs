use std::time::{Duration, Instant};

use crate::StreamCompressionParams;
use actix::prelude::*;
use actix_web_actors::ws;
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const CLIENT_TIMEOUT: Duration = Duration::from_secs(10);
