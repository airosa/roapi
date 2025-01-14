use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use columnq::table::TableSource;
use log::{error, info};
use snafu::prelude::*;
use tokio::sync::{Mutex, RwLock};
use tokio::time;

use crate::config::Config;
use crate::context::RawRoapiContext;
use crate::context::{ConcurrentRoapiContext, RoapiContext};
use crate::server;

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("Failed to build HTTP server: {source}"))]
    BuildHttpServer { source: server::http::Error },
    #[snafu(display("Failed to build FlightSQL server: {source}"))]
    BuildFlightSqlServer { source: server::flight_sql::Error },
}

// TODO: replace table reloader with the new concurrent refresh infra
pub struct TableReloader {
    reload_interval: Duration,
    ctx_ext: Arc<RwLock<RawRoapiContext>>,
    tables: Arc<Mutex<HashMap<String, TableSource>>>,
}

impl TableReloader {
    pub async fn run(self) {
        let mut interval = time::interval(self.reload_interval);
        loop {
            interval.tick().await;
            for (table_name, table) in self.tables.lock().await.iter() {
                match self.ctx_ext.load_table(table).await {
                    Ok(_) => {
                        info!("table {} reloaded", table_name);
                    }
                    Err(err) => {
                        error!("failed to reload table {}: {:?}", table_name, err);
                    }
                }
            }
        }
    }
}

pub struct Application {
    http_addr: std::net::SocketAddr,
    http_server: server::http::HttpApiServer,
    table_reloader: Option<TableReloader>,
    postgres_server: Box<dyn server::RunnableServer>,
    flight_sql_server: Box<dyn server::RunnableServer>,
}

impl Application {
    pub async fn build(config: Config) -> Result<Self, Error> {
        let default_host = std::env::var("HOST").unwrap_or_else(|_| "127.0.0.1".to_string());

        let handler_ctx = RawRoapiContext::new(&config, !config.disable_read_only)
            .await
            .expect("Failed to create Roapi context");

        let tables = config
            .tables
            .iter()
            .map(|t| (t.name.clone(), t.clone()))
            .collect::<HashMap<String, TableSource>>();
        let tables = Arc::new(Mutex::new(tables));

        if config.disable_read_only {
            let ctx_ext: Arc<ConcurrentRoapiContext> = Arc::new(RwLock::new(handler_ctx));
            let postgres_server = Box::new(
                server::postgres::PostgresServer::new(
                    ctx_ext.clone(),
                    &config,
                    default_host.clone(),
                )
                .await,
            );

            let table_reloader = config.reload_interval.map(|reload_interval| TableReloader {
                reload_interval,
                tables: tables.clone(),
                ctx_ext: ctx_ext.clone(),
            });

            let flight_sql_server = Box::new(
                server::flight_sql::RoapiFlightSqlServer::new(
                    ctx_ext.clone(),
                    &config,
                    default_host.clone(),
                )
                .await
                .context(BuildFlightSqlServerSnafu)?,
            );

            let (http_server, http_addr) =
                server::http::build_http_server(ctx_ext.clone(), tables, &config, default_host)
                    .await
                    .context(BuildHttpServerSnafu)?;

            let _handle = tokio::task::spawn(async move {
                loop {
                    if let Err(e) = ctx_ext.refresh_tables().await {
                        error!("Failed to refresh table: {:?}", e);
                    }
                    time::sleep(Duration::from_millis(1000)).await;
                }
            });

            Ok(Self {
                http_addr,
                http_server,
                postgres_server,
                flight_sql_server,
                table_reloader,
            })
        } else {
            let ctx_ext = Arc::new(handler_ctx);
            let postgres_server = Box::new(
                server::postgres::PostgresServer::new(
                    ctx_ext.clone(),
                    &config,
                    default_host.clone(),
                )
                .await,
            );
            let flight_sql_server = Box::new(
                server::flight_sql::RoapiFlightSqlServer::new(
                    ctx_ext.clone(),
                    &config,
                    default_host.clone(),
                )
                .await
                .context(BuildFlightSqlServerSnafu)?,
            );
            let (http_server, http_addr) = server::http::build_http_server::<RawRoapiContext>(
                ctx_ext,
                tables,
                &config,
                default_host,
            )
            .await
            .context(BuildHttpServerSnafu)?;

            Ok(Self {
                http_addr,
                http_server,
                postgres_server,
                flight_sql_server,
                table_reloader: None,
            })
        }
    }

    pub fn http_addr(&self) -> std::net::SocketAddr {
        self.http_addr
    }

    pub fn postgres_addr(&self) -> std::net::SocketAddr {
        self.postgres_server.addr()
    }

    pub fn flight_sql_addr(&self) -> std::net::SocketAddr {
        self.flight_sql_server.addr()
    }

    pub async fn run_until_stopped(self) -> Result<(), Error> {
        let postgres_server = self.postgres_server;
        info!(
            "🚀 Listening on {} for Postgres traffic...",
            postgres_server.addr()
        );
        tokio::spawn(async move {
            postgres_server
                .run()
                .await
                .expect("Failed to run postgres server");
        });

        let flight_sql_server = self.flight_sql_server;
        info!(
            "🚀 Listening on {} for FlightSQL traffic...",
            flight_sql_server.addr()
        );
        tokio::spawn(async move {
            flight_sql_server
                .run()
                .await
                .expect("Failed to run FlightSQL server");
        });

        if let Some(table_reloader) = self.table_reloader {
            tokio::spawn(async move {
                table_reloader.run().await;
            });
        }

        info!("🚀 Listening on {} for HTTP traffic...", self.http_addr);
        self.http_server.await.expect("Failed to start HTTP server");

        Ok(())
    }
}
