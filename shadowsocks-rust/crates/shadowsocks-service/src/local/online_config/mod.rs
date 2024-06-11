//! Online Config (SIP008)
//!
//! Online Configuration Delivery URL (https://shadowsocks.org/doc/sip008.html)

use std::{
    io,
    sync::Arc,
    time::{Duration, Instant},
};

use crate::{
    config::{Config, ConfigType},
    local::{context::ServiceContext, http::HttpClient, loadbalancing::PingBalancer},
};

use futures::StreamExt;
use http_body_util::BodyExt;
use log::{debug, error, trace, warn};
use mime::Mime;
use shadowsocks::config::ServerSource;
use tokio::time;

/// OnlineConfigService builder pattern
pub struct OnlineConfigServiceBuilder {
    context: Arc<ServiceContext>,
    config_url: String,
    balancer: PingBalancer,
    config_update_interval: Duration,
}

impl OnlineConfigServiceBuilder {
    /// Create a Builder
    pub fn new(context: Arc<ServiceContext>, config_url: String, balancer: PingBalancer) -> OnlineConfigServiceBuilder {
        OnlineConfigServiceBuilder {
            context,
            config_url,
            balancer,
            config_update_interval: Duration::from_secs(3600),
        }
    }

    /// Set update interval. Default is 3600s
    pub fn set_update_interval(&mut self, update_interval: Duration) {
        self.config_update_interval = update_interval;
    }

    /// Build OnlineConfigService
    pub async fn build(self) -> io::Result<OnlineConfigService> {
        let mut service = OnlineConfigService {
            context: self.context,
            http_client: HttpClient::new(),
            config_url: self.config_url,
            config_update_interval: self.config_update_interval,
            balancer: self.balancer,
        };

        // Run once after creation.
        service.run_once().await?;

        Ok(service)
    }
}

pub struct OnlineConfigService {
    context: Arc<ServiceContext>,
    http_client: HttpClient<String>,
    config_url: String,
    config_update_interval: Duration,
    balancer: PingBalancer,
}

impl OnlineConfigService {
    async fn run_once(&mut self) -> io::Result<()> {
        match time::timeout(Duration::from_secs(30), self.run_once_impl()).await {
            Ok(o) => o,
            Err(..) => {
                error!("server-loader task timeout, url: {}", self.config_url);
                Err(io::ErrorKind::TimedOut.into())
            }
        }
    }

    async fn run_once_impl(&mut self) -> io::Result<()> {
        static SHADOWSOCKS_USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

        let start_time = Instant::now();

        let req = match hyper::Request::builder()
            .header("User-Agent", SHADOWSOCKS_USER_AGENT)
            .method("GET")
            .uri(&self.config_url)
            .body(String::new())
        {
            Ok(r) => r,
            Err(err) => {
                error!("server-loader task failed to make hyper::Request, error: {}", err);
                return Err(io::Error::new(io::ErrorKind::Other, err));
            }
        };

        let rsp = match self.http_client.send_request(self.context.clone(), req, None).await {
            Ok(r) => r,
            Err(err) => {
                error!("server-loader task failed to get {}, error: {}", self.config_url, err);
                return Err(io::Error::new(io::ErrorKind::Other, err));
            }
        };

        let fetch_time = Instant::now();

        // Content-Type: application/json; charset=utf-8
        // mandatory in standard SIP008
        match rsp.headers().get("Content-Type") {
            Some(h) => match h.to_str() {
                Ok(hstr) => match hstr.parse::<Mime>() {
                    Ok(content_type) => {
                        if content_type.type_() == mime::APPLICATION
                            && content_type.subtype() == mime::JSON
                            && content_type.get_param(mime::CHARSET) == Some(mime::UTF_8)
                        {
                            trace!("checked Content-Type: {:?}", h);
                        } else {
                            warn!(
                                "Content-Type is not \"application/json; charset=utf-8\", which is mandatory in standard SIP008. found {:?}",
                                h
                            );
                        }
                    }
                    Err(err) => {
                        warn!("Content-Type parse failed, value: {:?}, error: {}", h, err);
                    }
                },
                Err(..) => {
                    warn!("Content-Type is not a UTF-8 string: {:?}", h);
                }
            },
            None => {
                warn!("missing Content-Type in SIP008 response from {}", self.config_url);
            }
        }

        let mut collected_body = Vec::new();
        if let Some(content_length) = rsp.headers().get(http::header::CONTENT_LENGTH) {
            if let Ok(content_length) = content_length.to_str() {
                if let Ok(content_length) = content_length.parse::<usize>() {
                    collected_body.reserve(content_length);
                }
            }
        };

        let mut body = rsp.into_data_stream();
        while let Some(data) = body.next().await {
            match data {
                Ok(data) => collected_body.extend_from_slice(&data),
                Err(err) => {
                    error!(
                        "server-loader task failed to read body, url: {}, error: {}",
                        self.config_url, err
                    );
                    return Err(io::Error::new(io::ErrorKind::Other, err));
                }
            }
        }

        let parsed_body = match String::from_utf8(collected_body) {
            Ok(b) => b,
            Err(..) => return Err(io::Error::new(io::ErrorKind::Other, "body contains non-utf8 bytes").into()),
        };

        let online_config = match Config::load_from_str(&parsed_body, ConfigType::OnlineConfig) {
            Ok(c) => c,
            Err(err) => {
                error!(
                    "server-loader task failed to load from url: {}, error: {}",
                    self.config_url, err
                );
                return Err(io::Error::new(io::ErrorKind::Other, err).into());
            }
        };

        if let Err(err) = online_config.check_integrity() {
            error!(
                "server-loader task failed to load from url: {}, error: {}",
                self.config_url, err
            );
            return Err(io::Error::new(io::ErrorKind::Other, err).into());
        }

        let after_read_time = Instant::now();

        // Merge with static servers
        let server_len = online_config.server.len();

        // Update into ping balancers
        if let Err(err) = self
            .balancer
            .reset_servers(online_config.server, &[ServerSource::OnlineConfig])
            .await
        {
            error!(
                "server-loader task failed to reset balancer, url: {}, error: {}",
                self.config_url, err
            );
            return Err(err);
        };

        let finish_time = Instant::now();

        debug!("server-loader task finished loading {} servers from url: {}, fetch time: {:?}, read time: {:?}, load time: {:?}, total time: {:?}",
            server_len,
            self.config_url,
            fetch_time - start_time,
            after_read_time - fetch_time,
            finish_time - after_read_time,
            finish_time - start_time,
        );

        Ok(())
    }

    /// Start service loop
    pub async fn run(mut self) -> io::Result<()> {
        debug!(
            "server-loader task started, url: {}, update interval: {:?}",
            self.config_url, self.config_update_interval
        );

        loop {
            time::sleep(self.config_update_interval).await;
            let _ = self.run_once().await;
        }
    }
}
