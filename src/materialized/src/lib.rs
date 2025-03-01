// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

//! A SQL stream processor built on top of [timely dataflow] and
//! [differential dataflow].
//!
//! [differential dataflow]: ../differential_dataflow/index.html
//! [timely dataflow]: ../timely/index.html

use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use compile_time_run::run_command_str;
use coord::PersistConfig;
use futures::StreamExt;
use openssl::ssl::{SslAcceptor, SslFiletype, SslMethod, SslVerifyMode};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio_stream::wrappers::TcpListenerStream;

use build_info::BuildInfo;
use coord::LoggingConfig;
use ore::metrics::MetricsRegistry;
use ore::now::SYSTEM_TIME;
use pid_file::PidFile;

use crate::mux::Mux;
use crate::server_metrics::Metrics;

pub mod http;
pub mod mux;
pub mod server_metrics;
pub mod telemetry;

// Disable jemalloc on macOS, as it is not well supported [0][1][2].
// The issues present as runaway latency on load test workloads that are
// comfortably handled by the macOS system allocator. Consider re-evaluating if
// jemalloc's macOS support improves.
//
// [0]: https://github.com/jemalloc/jemalloc/issues/26
// [1]: https://github.com/jemalloc/jemalloc/issues/843
// [2]: https://github.com/jemalloc/jemalloc/issues/1467
#[cfg(not(target_os = "macos"))]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

pub const BUILD_INFO: BuildInfo = BuildInfo {
    version: env!("CARGO_PKG_VERSION"),
    sha: run_command_str!(
        "sh",
        "-c",
        r#"if [ -n "$MZ_DEV_BUILD_SHA" ]; then
            echo "$MZ_DEV_BUILD_SHA"
        else
            # Unfortunately we need to suppress error messages from `git`, as
            # run_command_str will display no error message at all if we print
            # more than one line of output to stderr.
            git rev-parse --verify HEAD 2>/dev/null || {
                printf "error: unable to determine Git SHA; " >&2
                printf "either build from working Git clone " >&2
                printf "(see https://materialize.com/docs/install/#build-from-source), " >&2
                printf "or specify SHA manually by setting MZ_DEV_BUILD_SHA environment variable" >&2
                exit 1
            }
        fi"#
    ),
    time: run_command_str!("date", "-u", "+%Y-%m-%dT%H:%M:%SZ"),
    target_triple: env!("TARGET_TRIPLE"),
};

/// Configuration for a `materialized` server.
#[derive(Debug, Clone)]
pub struct Config {
    // === Timely and Differential worker options. ===
    /// The number of Timely worker threads that this process should host.
    pub workers: usize,
    /// The Timely worker configuration.
    pub timely_worker: timely::WorkerConfig,

    // === Performance tuning options. ===
    pub logging: Option<LoggingConfig>,
    /// The frequency at which to update introspection.
    pub introspection_frequency: Duration,
    /// The historical window in which distinctions are maintained for
    /// arrangements.
    ///
    /// As arrangements accept new timestamps they may optionally collapse prior
    /// timestamps to the same value, retaining their effect but removing their
    /// distinction. A large value or `None` results in a large amount of
    /// historical detail for arrangements; this increases the logical times at
    /// which they can be accurately queried, but consumes more memory. A low
    /// value reduces the amount of memory required but also risks not being
    /// able to use the arrangement in a query that has other constraints on the
    /// timestamps used (e.g. when joined with other arrangements).
    pub logical_compaction_window: Option<Duration>,
    /// The interval at which sources should be timestamped.
    pub timestamp_frequency: Duration,

    // === Connection options. ===
    /// The IP address and port to listen on.
    pub listen_addr: SocketAddr,
    /// The IP address and port to serve the "third party" metrics registry from.
    pub third_party_metrics_listen_addr: Option<SocketAddr>,
    /// TLS encryption configuration.
    pub tls: Option<TlsConfig>,

    // === Storage options. ===
    /// The directory in which `materialized` should store its own metadata.
    pub data_directory: PathBuf,

    // === Mode switches. ===
    /// An optional symbiosis endpoint. See the
    /// [`symbiosis`](../symbiosis/index.html) crate for details.
    pub symbiosis_url: Option<String>,
    /// Whether to permit usage of experimental features.
    pub experimental_mode: bool,
    /// Whether to enable catalog-only mode.
    pub disable_user_indexes: bool,
    /// Whether to run in safe mode.
    pub safe_mode: bool,
    /// Telemetry configuration.
    pub telemetry: Option<TelemetryConfig>,
    /// The place where the server's metrics will be reported from.
    pub metrics_registry: MetricsRegistry,
    /// Configuration of the persistence runtime and features.
    pub persist: PersistConfig,
}

/// Configures TLS encryption for connections.
#[derive(Debug, Clone)]
pub struct TlsConfig {
    /// The TLS mode to use.
    pub mode: TlsMode,
    /// The path to the TLS certificate.
    pub cert: PathBuf,
    /// The path to the TLS key.
    pub key: PathBuf,
}

/// Configures how strictly to enforce TLS encryption and authentication.
#[derive(Debug, Clone)]
pub enum TlsMode {
    /// Require that all clients connect with TLS, but do not require that they
    /// present a client certificate.
    Require,
    /// Require that clients connect with TLS and present a certificate that
    /// is signed by the specified CA.
    VerifyCa {
        /// The path to a TLS certificate authority.
        ca: PathBuf,
    },
    /// Like [`TlsMode::VerifyCa`], but the `cn` (Common Name) field of the
    /// certificate must additionally match the user named in the connection
    /// request.
    VerifyFull {
        /// The path to a TLS certificate authority.
        ca: PathBuf,
    },
}

/// Telemetry configuration.
#[derive(Debug, Clone)]
pub struct TelemetryConfig {
    /// The domain hosting the telemetry server.
    pub domain: String,
    /// The interval at which to report telemetry data.
    pub interval: Duration,
}

/// Start a `materialized` server.
pub async fn serve(config: Config) -> Result<Server, anyhow::Error> {
    let workers = config.workers;

    // Validate TLS configuration, if present.
    let (pgwire_tls, http_tls) = match &config.tls {
        None => (None, None),
        Some(tls_config) => {
            let context = {
                // Mozilla publishes three presets: old, intermediate, and modern. They
                // recommend the intermediate preset for general purpose servers, which
                // is what we use, as it is compatible with nearly every client released
                // in the last five years but does not include any known-problematic
                // ciphers. We once tried to use the modern preset, but it was
                // incompatible with Fivetran, and presumably other JDBC-based tools.
                let mut builder = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls())?;
                if let TlsMode::VerifyCa { ca } | TlsMode::VerifyFull { ca } = &tls_config.mode {
                    builder.set_ca_file(ca)?;
                    builder.set_verify(SslVerifyMode::PEER | SslVerifyMode::FAIL_IF_NO_PEER_CERT);
                }
                builder.set_certificate_file(&tls_config.cert, SslFiletype::PEM)?;
                builder.set_private_key_file(&tls_config.key, SslFiletype::PEM)?;
                builder.build().into_context()
            };
            let pgwire_tls = pgwire::TlsConfig {
                context: context.clone(),
                mode: match tls_config.mode {
                    TlsMode::Require | TlsMode::VerifyCa { .. } => pgwire::TlsMode::Require,
                    TlsMode::VerifyFull { .. } => pgwire::TlsMode::VerifyUser,
                },
            };
            let http_tls = http::TlsConfig {
                context,
                mode: match tls_config.mode {
                    TlsMode::Require | TlsMode::VerifyCa { .. } => http::TlsMode::Require,
                    TlsMode::VerifyFull { .. } => http::TlsMode::AssumeUser,
                },
            };
            (Some(pgwire_tls), Some(http_tls))
        }
    };

    // Attempt to acquire PID file lock.
    let pid_file = PidFile::open(config.data_directory.join("materialized.pid"))?;

    // Initialize network listener.
    let listener = TcpListener::bind(&config.listen_addr).await?;
    let local_addr = listener.local_addr()?;

    // Initialize dataflow server.
    let (dataflow_server, dataflow_client) = dataflow::serve(dataflow::Config {
        workers,
        timely_config: timely::Config {
            communication: timely::CommunicationConfig::Process(workers),
            worker: timely::WorkerConfig::default(),
        },
        experimental_mode: config.experimental_mode,
        now: SYSTEM_TIME.clone(),
        metrics_registry: config.metrics_registry.clone(),
    })?;

    // Initialize coordinator.
    let (coord_handle, coord_client) = coord::serve(coord::Config {
        dataflow_client,
        symbiosis_url: config.symbiosis_url.as_deref(),
        logging: config.logging,
        data_directory: &config.data_directory,
        timestamp_frequency: config.timestamp_frequency,
        logical_compaction_window: config.logical_compaction_window,
        experimental_mode: config.experimental_mode,
        disable_user_indexes: config.disable_user_indexes,
        safe_mode: config.safe_mode,
        build_info: &BUILD_INFO,
        metrics_registry: config.metrics_registry.clone(),
        persist: config.persist,
        now: SYSTEM_TIME.clone(),
    })
    .await?;

    // Register metrics.
    let mut metrics_registry = config.metrics_registry;
    let metrics =
        Metrics::register_with(&mut metrics_registry, workers, coord_handle.start_instant());

    // Listen on the third-party metrics port if we are configured for it.
    if let Some(third_party_addr) = config.third_party_metrics_listen_addr {
        tokio::spawn({
            let server = http::ThirdPartyServer::new(metrics_registry.clone(), metrics.clone());
            async move {
                server.serve(third_party_addr).await;
            }
        });
    }

    // Launch task to serve connections.
    //
    // The lifetime of this task is controlled by a trigger that activates on
    // drop. Draining marks the beginning of the server shutdown process and
    // indicates that new user connections (i.e., pgwire and HTTP connections)
    // should be rejected. Once all existing user connections have gracefully
    // terminated, this task exits.
    let (drain_trigger, drain_tripwire) = oneshot::channel();
    tokio::spawn({
        let pgwire_server = pgwire::Server::new(pgwire::Config {
            tls: pgwire_tls,
            coord_client: coord_client.clone(),
            metrics_registry: &metrics_registry,
        });
        let http_server = http::Server::new(http::Config {
            tls: http_tls,
            coord_client: coord_client.clone(),
            metrics_registry,
            global_metrics: metrics,
            pgwire_metrics: pgwire_server.metrics(),
        });
        let mut mux = Mux::new();
        mux.add_handler(pgwire_server);
        mux.add_handler(http_server);
        async move {
            // TODO(benesch): replace with `listener.incoming()` if that is
            // restored when the `Stream` trait stabilizes.
            let mut incoming = TcpListenerStream::new(listener);
            mux.serve(incoming.by_ref().take_until(drain_tripwire))
                .await;
        }
    });

    // Start telemetry reporting loop.
    if let Some(telemetry) = config.telemetry {
        let config = telemetry::Config {
            domain: telemetry.domain,
            interval: telemetry.interval,
            cluster_id: coord_handle.cluster_id(),
            workers,
            coord_client,
        };
        tokio::spawn(async move { telemetry::report_loop(config).await });
    }

    Ok(Server {
        local_addr,
        _pid_file: pid_file,
        _drain_trigger: drain_trigger,
        _coord_handle: coord_handle,
        _dataflow_server: dataflow_server,
    })
}

/// A running `materialized` server.
pub struct Server {
    local_addr: SocketAddr,
    _pid_file: PidFile,
    // Drop order matters for these fields.
    _drain_trigger: oneshot::Sender<()>,
    _coord_handle: coord::Handle,
    _dataflow_server: dataflow::Server,
}

impl Server {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
}
