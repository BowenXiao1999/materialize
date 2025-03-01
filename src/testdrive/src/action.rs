// Copyright Materialize, Inc. and contributors. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::cmp;
use std::collections::{BTreeSet, HashMap};
use std::env;
use std::fs;
use std::future::Future;
use std::net::ToSocketAddrs;
use std::path::PathBuf;
use std::time::Duration;

use ::http::Uri;
use async_trait::async_trait;
use aws_sdk_kinesis::Client as KinesisClient;
use aws_sdk_s3::Client as S3Client;
use aws_sdk_sqs::Client as SqsClient;
use aws_types::credentials::ProvideCredentials;
use futures::future::FutureExt;
use lazy_static::lazy_static;
use ore::result::ResultExt as _;
use rand::Rng;
use rdkafka::ClientConfig;
use regex::{Captures, Regex};
use url::Url;

use mz_aws_util::config::AwsConfig;
use ore::retry::Retry;

use crate::error::{DynError, Error, InputError, ResultExt};
use crate::parser::{Command, PosCommand, SqlOutput};
use crate::util;

mod avro_ocf;
mod file;
mod http;
mod kafka;
mod kinesis;
mod postgres;
mod protobuf;
mod psql;
mod s3;
mod schema_registry;
mod sleep;
mod sql;
mod sql_server;

const DEFAULT_REGEX_REPLACEMENT: &str = "<regex_match>";

/// User-settable configuration parameters.
#[derive(Debug)]
pub struct Config {
    /// The address of the Kafka broker that testdrive will interact with.
    pub kafka_addr: String,
    /// Arbitrary rdkafka options for testdrive to use when connecting to the
    /// Kafka broker.
    pub kafka_opts: Vec<(String, String)>,
    /// The URL of the schema registry that testdrive will connect to.
    pub schema_registry_url: Url,
    /// An optional path to a TLS certificate that testdrive will present when
    /// performing client authentication.
    pub cert_path: Option<String>,
    /// An optional password for the TLS certificate.
    pub cert_pass: Option<String>,
    /// The account for testdrive to use when connecting to AWS.
    pub aws_account: String,
    /// The configuration for testdrive to use when connecting to AWS.
    pub aws_config: AwsConfig,
    /// The connection parameters for the materialized instance that testdrive
    /// will connect to.
    pub materialized_pgconfig: tokio_postgres::Config,
    /// An optional path to the catalog file for the materialized instance.
    ///
    /// If present, testdrive will periodically verify that the on-disk catalog
    /// matches its expectations.
    pub materialized_catalog_path: Option<PathBuf>,
    /// Whether to reset Materialized and AWS's state at the start of each script.
    pub reset: bool,
    /// Emit Buildkite-specific markup.
    pub ci_output: bool,
    /// The default timeout to use for any operation that is retried.
    pub default_timeout: Duration,

    /// Starting backoff value to use when retrying
    pub initial_backoff: Duration,

    /// Backoff factor to use when retrying
    pub backoff_factor: f64,

    /// A random number to distinguish each run of a testdrive script.
    pub seed: Option<u32>,
    /// Force the use of a specific temporary directory
    pub temp_dir: Option<String>,
}

pub struct State {
    seed: u32,
    temp_path: PathBuf,
    _tempfile_handle: Option<tempfile::TempDir>,
    materialized_catalog_path: Option<PathBuf>,
    materialized_addr: String,
    materialized_user: String,
    pgclient: tokio_postgres::Client,
    schema_registry_url: Url,
    ccsr_client: ccsr::Client,
    kafka_addr: String,
    kafka_admin: rdkafka::admin::AdminClient<rdkafka::client::DefaultClientContext>,
    kafka_admin_opts: rdkafka::admin::AdminOptions,
    kafka_config: ClientConfig,
    kafka_producer: rdkafka::producer::FutureProducer<rdkafka::client::DefaultClientContext>,
    kafka_topics: HashMap<String, usize>,
    aws_account: String,
    aws_config: AwsConfig,
    kinesis_client: KinesisClient,
    kinesis_stream_names: Vec<String>,
    s3_client: S3Client,
    s3_buckets_created: BTreeSet<String>,
    sqs_client: SqsClient,
    sqs_queues_created: BTreeSet<String>,
    default_timeout: Duration,
    initial_backoff: Duration,
    backoff_factor: f64,
    postgres_clients: HashMap<String, tokio_postgres::Client>,
    sql_server_clients:
        HashMap<String, tiberius::Client<tokio_util::compat::Compat<tokio::net::TcpStream>>>,
}

#[derive(Clone)]
pub struct Context {
    timeout: Duration,
    initial_backoff: Duration,
    backoff_factor: f64,
    regex: Option<Regex>,
    regex_replacement: String,
}

impl State {
    pub fn aws_endpoint(&self) -> String {
        match self.aws_config.endpoint() {
            None => String::new(),
            Some(endpoint) => {
                let mut uri = Uri::builder().build().unwrap();
                endpoint.set_endpoint(&mut uri, None);
                uri.to_string()
            }
        }
    }

    pub fn aws_region(&self) -> &str {
        self.aws_config.region().map(|r| r.as_ref()).unwrap_or("")
    }

    pub async fn reset_materialized(&mut self) -> Result<(), Error> {
        for row in self
            .pgclient
            .query("SHOW DATABASES", &[])
            .await
            .err_ctx("resetting materialized state: SHOW DATABASES")?
        {
            let db_name: String = row.get(0);
            let query = format!("DROP DATABASE {}", db_name);
            sql::print_query(&query);
            self.pgclient.batch_execute(&query).await.err_ctx(format!(
                "resetting materialized state: DROP DATABASE {}",
                db_name,
            ))?;
        }
        self.pgclient
            .batch_execute("CREATE DATABASE materialize")
            .await
            .err_ctx("resetting materialized state: CREATE DATABASE materialize")?;

        // Attempt to remove all users but the current user. Old versions of
        // Materialize did not support roles, so this degrades gracefully if
        // mz_roles does not exist.
        if let Ok(rows) = self.pgclient.query("SELECT name FROM mz_roles", &[]).await {
            for row in rows {
                let role_name: String = row.get(0);
                if role_name == self.materialized_user || role_name.starts_with("mz_") {
                    continue;
                }
                let query = format!("DROP ROLE {}", role_name);
                sql::print_query(&query);
                self.pgclient.batch_execute(&query).await.err_ctx(format!(
                    "resetting materialized state: DROP ROLE {}",
                    role_name,
                ))?;
            }
        }

        Ok(())
    }

    /// Delete the Kinesis streams created for this run of testdrive.
    pub async fn reset_kinesis(&mut self) -> Result<(), Error> {
        if self.kinesis_stream_names.is_empty() {
            return Ok(());
        }
        println!(
            "Deleting Kinesis streams {}",
            self.kinesis_stream_names.join(", ")
        );
        for stream_name in &self.kinesis_stream_names {
            self.kinesis_client
                .delete_stream()
                .enforce_consumer_deletion(true)
                .stream_name(stream_name)
                .send()
                .await
                .err_ctx(format!("deleting Kinesis stream: {}", stream_name))?;
        }
        Ok(())
    }

    /// Delete S3 buckets that were created in this run
    pub async fn reset_s3(&mut self) -> Result<(), Error> {
        let mut errors: Vec<DynError> = Vec::new();
        for bucket in &self.s3_buckets_created {
            if let Err(e) = self.delete_bucket_objects(bucket.clone()).await {
                errors.push(e.into());
            }

            let res = self.s3_client.delete_bucket().bucket(bucket).send().await;

            if let Err(e) = res {
                errors.push(e.into());
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(Error::General {
                ctx: format!("deleting S3 buckets: {} errors", errors.len()),
                causes: errors,
                hints: Vec::new(),
            })
        }
    }

    async fn delete_bucket_objects(&self, bucket: String) -> Result<(), Error> {
        Retry::default()
            .max_duration(self.default_timeout)
            .retry(|_| async {
                // loop until error or response has no continuation token
                let mut continuation_token = None;
                loop {
                    let response = self
                        .s3_client
                        .list_objects_v2()
                        .bucket(&bucket)
                        .set_continuation_token(continuation_token)
                        .send()
                        .await
                        .with_err_ctx(|| format!("listing objects for bucket {}", bucket))?;

                    if let Some(objects) = response.contents {
                        for obj in objects {
                            self.s3_client
                                .delete_object()
                                .bucket(&bucket)
                                .key(obj.key.as_ref().unwrap())
                                .send()
                                .await
                                .with_err_ctx(|| {
                                    format!("deleting object {}/{}", bucket, obj.key.unwrap())
                                })?;
                        }
                    }

                    if response.next_continuation_token.is_none() {
                        return Ok(());
                    }
                    continuation_token = response.next_continuation_token;
                }
            })
            .await
    }

    pub async fn reset_sqs(&self) -> Result<(), Error> {
        Retry::default()
            .max_duration(self.default_timeout)
            .retry(|_| async {
                for queue_url in &self.sqs_queues_created {
                    self.sqs_client
                        .delete_queue()
                        .queue_url(queue_url)
                        .send()
                        .await
                        .with_err_ctx(|| format!("Deleting sqs queue: {}", queue_url))?;
                }

                Ok(())
            })
            .await
    }
}

pub struct PosAction {
    pub pos: usize,
    pub action: Box<dyn Action + Send + Sync>,
}

#[async_trait]
pub trait Action {
    async fn undo(&self, state: &mut State) -> Result<(), String>;
    async fn redo(&self, state: &mut State) -> Result<(), String>;
}

pub trait SyncAction: Send + Sync {
    fn undo(&self, state: &mut State) -> Result<(), String>;
    fn redo(&self, state: &mut State) -> Result<(), String>;
}

#[async_trait]
impl<T> Action for T
where
    T: SyncAction,
{
    async fn undo(&self, state: &mut State) -> Result<(), String> {
        tokio::task::block_in_place(|| self.undo(state))
    }

    async fn redo(&self, state: &mut State) -> Result<(), String> {
        tokio::task::block_in_place(|| self.redo(state))
    }
}

pub async fn build(cmds: Vec<PosCommand>, state: &State) -> Result<Vec<PosAction>, Error> {
    let mut out = Vec::new();
    let mut vars = HashMap::new();

    let mut context = Context {
        timeout: state.default_timeout,
        initial_backoff: state.initial_backoff,
        backoff_factor: state.backoff_factor,
        regex: None,
        regex_replacement: DEFAULT_REGEX_REPLACEMENT.to_string(),
    };

    vars.insert("testdrive.kafka-addr".into(), state.kafka_addr.clone());
    vars.insert(
        "testdrive.kafka-addr-resolved".into(),
        state
            .kafka_addr
            .to_socket_addrs()
            .ok()
            .and_then(|mut addrs| addrs.next())
            .map(|addr| addr.to_string())
            .unwrap_or_else(|| "#RESOLUTION-FAILURE#".into()),
    );
    vars.insert(
        "testdrive.schema-registry-url".into(),
        state.schema_registry_url.to_string(),
    );
    vars.insert("testdrive.seed".into(), state.seed.to_string());
    vars.insert(
        "testdrive.temp-dir".into(),
        state.temp_path.display().to_string(),
    );
    vars.insert("testdrive.aws-region".into(), state.aws_region().into());
    vars.insert("testdrive.aws-endpoint".into(), state.aws_endpoint());
    vars.insert("testdrive.aws-account".into(), state.aws_account.clone());
    {
        let aws_credentials = state
            .aws_config
            .credentials_provider()
            .provide_credentials()
            .await
            .err_ctx("fetching AWS credentials")?;
        vars.insert(
            "testdrive.aws-access-key-id".into(),
            aws_credentials.access_key_id().to_owned(),
        );
        vars.insert(
            "testdrive.aws-secret-access-key".into(),
            aws_credentials.secret_access_key().to_owned(),
        );
        vars.insert(
            "testdrive.aws-token".into(),
            aws_credentials
                .session_token()
                .map(|token| token.to_owned())
                .unwrap_or_else(String::new),
        );
    }
    vars.insert(
        "testdrive.materialized-addr".into(),
        state.materialized_addr.clone(),
    );
    vars.insert(
        "testdrive.materialized-user".into(),
        state.materialized_user.clone(),
    );

    for (key, value) in env::vars() {
        vars.insert(format!("env.{}", key), value);
    }

    for cmd in cmds {
        let pos = cmd.pos;
        let wrap_err = |e| InputError { msg: e, pos };

        // Substitute variables at startup except for the command-specific ones
        // Those will be substituted at runtime
        let ignore_prefix = match &cmd.command {
            Command::Builtin(builtin) => Some(builtin.name.clone()),
            _ => None,
        };
        let subst = |msg: &str| substitute_vars(msg, &vars, &ignore_prefix).map_err(wrap_err);

        let action: Box<dyn Action + Send + Sync> = match cmd.command {
            Command::Builtin(mut builtin) => {
                for val in builtin.args.values_mut() {
                    *val = subst(val)?;
                }
                for line in &mut builtin.input {
                    *line = subst(line)?;
                }
                match builtin.name.as_ref() {
                    "avro-ocf-write" => Box::new(avro_ocf::build_write(builtin).map_err(wrap_err)?),
                    "avro-ocf-append" => {
                        Box::new(avro_ocf::build_append(builtin).map_err(wrap_err)?)
                    }
                    "avro-ocf-verify" => Box::new(
                        avro_ocf::build_verify(builtin, context.clone()).map_err(wrap_err)?,
                    ),
                    "file-append" => Box::new(file::build_append(builtin).map_err(wrap_err)?),
                    "file-delete" => Box::new(file::build_delete(builtin).map_err(wrap_err)?),
                    "http-request" => Box::new(http::build_request(builtin).map_err(wrap_err)?),
                    "kafka-add-partitions" => {
                        Box::new(kafka::build_add_partitions(builtin).map_err(wrap_err)?)
                    }
                    "kafka-create-topic" => {
                        Box::new(kafka::build_create_topic(builtin).map_err(wrap_err)?)
                    }
                    "kafka-ingest" => Box::new(kafka::build_ingest(builtin).map_err(wrap_err)?),
                    "kafka-verify" => {
                        Box::new(kafka::build_verify(builtin, context.clone()).map_err(wrap_err)?)
                    }
                    "kinesis-create-stream" => {
                        Box::new(kinesis::build_create_stream(builtin).map_err(wrap_err)?)
                    }
                    "kinesis-update-shards" => {
                        Box::new(kinesis::build_update_shards(builtin).map_err(wrap_err)?)
                    }
                    "kinesis-ingest" => Box::new(kinesis::build_ingest(builtin).map_err(wrap_err)?),
                    "kinesis-verify" => Box::new(kinesis::build_verify(builtin).map_err(wrap_err)?),
                    "postgres-connect" => {
                        Box::new(postgres::build_connect(builtin).map_err(wrap_err)?)
                    }
                    "postgres-execute" => {
                        Box::new(postgres::build_execute(builtin).map_err(wrap_err)?)
                    }
                    "postgres-verify-slot" => {
                        Box::new(postgres::build_verify_slot(builtin).map_err(wrap_err)?)
                    }
                    "protobuf-compile-descriptors" => {
                        Box::new(protobuf::build_compile_descriptors(builtin).map_err(wrap_err)?)
                    }
                    "psql-execute" => Box::new(psql::build_execute(builtin).map_err(wrap_err)?),
                    "schema-registry-publish" => {
                        Box::new(schema_registry::build_publish(builtin).map_err(wrap_err)?)
                    }
                    "schema-registry-wait-schema" => Box::new(
                        schema_registry::build_wait(builtin, context.clone()).map_err(wrap_err)?,
                    ),
                    "sql-server-connect" => {
                        Box::new(sql_server::build_connect(builtin).map_err(wrap_err)?)
                    }
                    "sql-server-execute" => {
                        Box::new(sql_server::build_execute(builtin).map_err(wrap_err)?)
                    }
                    "random-sleep" => {
                        Box::new(sleep::build_random_sleep(builtin).map_err(wrap_err)?)
                    }
                    "s3-create-bucket" => {
                        Box::new(s3::build_create_bucket(builtin).map_err(wrap_err)?)
                    }
                    "s3-put-object" => Box::new(s3::build_put_object(builtin).map_err(wrap_err)?),
                    "s3-delete-objects" => {
                        Box::new(s3::build_delete_object(builtin).map_err(wrap_err)?)
                    }
                    "s3-add-notifications" => {
                        Box::new(s3::build_add_notifications(builtin).map_err(wrap_err)?)
                    }
                    "set-regex" => {
                        context.regex = Some(builtin.args.parse("match").map_err(wrap_err)?);
                        context.regex_replacement = match builtin.args.opt_string("replacement") {
                            None => DEFAULT_REGEX_REPLACEMENT.into(),
                            Some(replacement) => replacement,
                        };
                        continue;
                    }
                    "set-sql-timeout" => {
                        let duration = builtin.args.string("duration").map_err(wrap_err)?;
                        if duration.to_lowercase() == "default" {
                            context.timeout = state.default_timeout;
                        } else {
                            // do not allow the timeout to be set below the default
                            context.timeout = cmp::max(
                                repr::util::parse_duration(&duration)
                                    .map_err_to_string()
                                    .map_err(wrap_err)?,
                                state.default_timeout,
                            );
                        }
                        continue;
                    }
                    "set-execution-count" => {
                        // Skip, has already been handled
                        continue;
                    }
                    "sleep-is-probably-flaky-i-have-justified-my-need-with-a-comment" => {
                        Box::new(sleep::build_sleep(builtin).map_err(wrap_err)?)
                    }
                    "set" => {
                        for (key, val) in builtin.args {
                            if val.is_empty() {
                                vars.insert(key, builtin.input.join("\n"));
                            } else {
                                vars.insert(key, val);
                            }
                        }
                        continue;
                    }
                    _ => {
                        return Err(InputError {
                            msg: format!("unknown built-in command {}", builtin.name),
                            pos: cmd.pos,
                        }
                        .into());
                    }
                }
            }
            Command::Sql(mut sql) => {
                sql.query = subst(&sql.query)?;
                if let SqlOutput::Full { expected_rows, .. } = &mut sql.expected_output {
                    for row in expected_rows {
                        for col in row {
                            *col = subst(col)?;
                        }
                    }
                }
                Box::new(sql::build_sql(sql, context.clone()).map_err(wrap_err)?)
            }
            Command::FailSql(mut sql) => {
                sql.query = subst(&sql.query)?;
                sql.expected_error = subst(&sql.expected_error)?;
                Box::new(sql::build_fail_sql(sql, context.clone()).map_err(wrap_err)?)
            }
        };
        out.push(PosAction {
            pos: cmd.pos,
            action,
        })
    }
    Ok(out)
}

/// Substituted `${}`-delimited variables from `vars` into `msg`
fn substitute_vars(
    msg: &str,
    vars: &HashMap<String, String>,
    ignore_prefix: &Option<String>,
) -> Result<String, String> {
    lazy_static! {
        static ref RE: Regex = Regex::new(r"\$\{([^}]+)\}").unwrap();
    }
    let mut err = None;
    let out = RE.replace_all(msg, |caps: &Captures| {
        let name = &caps[1];
        if let Some(ignore_prefix) = &ignore_prefix {
            if name.starts_with(format!("{}.", ignore_prefix).as_str()) {
                // Do not subsitute, leave original variable name in place
                return caps.get(0).unwrap().as_str().to_string();
            }
        }

        if let Some(val) = vars.get(name) {
            val.to_string()
        } else {
            err = Some(format!("unknown variable: {}", name));
            "#VAR-MISSING#".to_string()
        }
    });
    match err {
        Some(err) => Err(err),
        None => Ok(out.into_owned()),
    }
}

/// Initializes a [`State`] object by connecting to the various external
/// services specified in `config`.
///
/// Returns the initialized `State` and a cleanup future. The cleanup future
/// should be `await`ed only *after* dropping the `State` to check whether any
/// errors occured while dropping the `State`. This awkward API is a workaround
/// for the lack of `AsyncDrop` support in Rust.
pub async fn create_state(
    config: &Config,
) -> Result<(State, impl Future<Output = Result<(), Error>>), Error> {
    let seed = config.seed.unwrap_or_else(|| rand::thread_rng().gen());

    let (_tempfile_handle, temp_path) = match &config.temp_dir {
        Some(temp_dir) => {
            fs::create_dir_all(temp_dir).err_ctx("creating temporary directory")?;
            (None, PathBuf::from(&temp_dir))
        }
        _ => {
            // Stash the tempfile object so that it does not go out of scope and delete
            // the tempdir prematurely
            let tempfile_handle = tempfile::tempdir().err_ctx("creating temporary directory")?;
            let temp_path = tempfile_handle.path().to_path_buf();
            (Some(tempfile_handle), temp_path)
        }
    };

    let materialized_catalog_path = if let Some(path) = &config.materialized_catalog_path {
        match fs::metadata(&path) {
            Ok(m) if !m.is_file() => {
                return Err(Error::message(
                    "materialized catalog path is not a regular file",
                ))
            }
            Ok(_) => Some(path.to_path_buf()),
            Err(e) => return Err(e).err_ctx("opening materialized catalog path"),
        }
    } else {
        None
    };

    let (materialized_addr, materialized_user, pgclient, pgconn_task) = {
        let materialized_url = util::postgres::config_url(&config.materialized_pgconfig)?;
        let (pgclient, pgconn) = config
            .materialized_pgconfig
            .connect(tokio_postgres::NoTls)
            .await
            .err_hint(
                "opening SQL connection",
                &[
                    format!("connection string: {}", materialized_url),
                    "are you running the materialized server?".into(),
                ],
            )?;
        let pgconn_task = tokio::spawn(pgconn).map(|join| {
            join.expect("pgconn_task unexpectedly canceled")
                .err_ctx("running SQL connection")
        });

        // Old versions of Materialize did not support `current_user`, so we
        // fail gracefully.
        let materialized_user = match pgclient.query_one("SELECT current_user", &[]).await {
            Ok(row) => row.get(0),
            Err(_) => "<unknown user>".to_owned(),
        };

        let materialized_addr = format!(
            "{}:{}",
            materialized_url.host_str().unwrap(),
            materialized_url.port().unwrap()
        );
        (materialized_addr, materialized_user, pgclient, pgconn_task)
    };

    let schema_registry_url = config.schema_registry_url.to_owned();

    let ccsr_client = {
        let mut ccsr_config = ccsr::ClientConfig::new(schema_registry_url.clone());

        if let Some(cert_path) = &config.cert_path {
            let cert = fs::read(cert_path)
                .err_hint("reading cert", &[format!("is {} readable?", cert_path)])?;
            let pass = config.cert_pass.as_deref().unwrap_or("").to_owned();
            let ident = ccsr::tls::Identity::from_pkcs12_der(cert, pass).err_hint(
                "reading keystore file as pkcs12",
                &[format!("is {} a valid pkcs12 file?", cert_path)],
            )?;
            ccsr_config = ccsr_config.identity(ident);
        }

        ccsr_config.build().err_ctx("Creating CCSR client")?
    };

    let (kafka_addr, kafka_admin, kafka_admin_opts, kafka_producer, kafka_topics, kafka_config) = {
        use rdkafka::admin::{AdminClient, AdminOptions};
        use rdkafka::client::DefaultClientContext;
        use rdkafka::producer::FutureProducer;

        let mut kafka_config = ClientConfig::new();
        kafka_config.set("bootstrap.servers", &config.kafka_addr);
        kafka_config.set("group.id", "materialize-testdrive");
        kafka_config.set("auto.offset.reset", "earliest");
        kafka_config.set("isolation.level", "read_committed");
        if let Some(cert_path) = &config.cert_path {
            kafka_config.set("security.protocol", "ssl");
            kafka_config.set("ssl.keystore.location", cert_path);
            if let Some(cert_pass) = &config.cert_pass {
                kafka_config.set("ssl.keystore.password", cert_pass);
            }
        }
        for (key, value) in &config.kafka_opts {
            kafka_config.set(key, value);
        }

        let admin: AdminClient<DefaultClientContext> = kafka_config.create().err_hint(
            "opening Kafka connection",
            &[format!("connection string: {}", config.kafka_addr)],
        )?;

        let admin_opts = AdminOptions::new().operation_timeout(Some(config.default_timeout));

        let producer: FutureProducer = kafka_config.create().err_hint(
            "opening Kafka producer connection",
            &[format!("connection string: {}", config.kafka_addr)],
        )?;

        let topics = HashMap::new();

        (
            config.kafka_addr.to_owned(),
            admin,
            admin_opts,
            producer,
            topics,
            kafka_config,
        )
    };

    let kinesis_client =
        mz_aws_util::kinesis::client(&config.aws_config).err_ctx("creating Kinesis client")?;
    let s3_client = mz_aws_util::s3::client(&config.aws_config).err_ctx("creating S3 client")?;
    let sqs_client = mz_aws_util::sqs::client(&config.aws_config).err_ctx("creating SQS client")?;

    let state = State {
        seed,
        temp_path,
        _tempfile_handle,
        materialized_catalog_path,
        materialized_addr,
        materialized_user,
        pgclient,
        schema_registry_url,
        ccsr_client,
        kafka_addr,
        kafka_admin,
        kafka_admin_opts,
        kafka_config,
        kafka_producer,
        kafka_topics,
        aws_account: config.aws_account.clone(),
        aws_config: config.aws_config.clone(),
        kinesis_client,
        kinesis_stream_names: Vec::new(),
        s3_client,
        s3_buckets_created: BTreeSet::new(),
        sqs_client,
        sqs_queues_created: BTreeSet::new(),
        default_timeout: config.default_timeout,
        initial_backoff: config.initial_backoff,
        backoff_factor: config.backoff_factor,
        postgres_clients: HashMap::new(),
        sql_server_clients: HashMap::new(),
    };
    Ok((state, pgconn_task))
}
