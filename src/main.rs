#![cfg_attr(feature="clippy", feature(plugin))]
#![cfg_attr(feature="clippy", plugin(clippy))]

#[macro_use]
extern crate log;
extern crate clockpro_cache;
extern crate bytes;
extern crate clap;
extern crate env_logger;
extern crate mio;
extern crate nix;
extern crate privdrop;
extern crate rand;
extern crate siphasher;
extern crate slab;
extern crate toml;

#[cfg(feature = "webservice")]
extern crate hyper;

#[macro_use]
extern crate prometheus;

mod cache;
mod client_query;
mod client;
mod config;
mod dns;
mod resolver;
mod tcp_listener;
mod udp_listener;
mod varz;

#[cfg(feature = "webservice")]
mod webservice;

use cache::Cache;
use clap::{Arg, App};
use config::Config;
use privdrop::PrivDrop;
use resolver::*;
use std::net::UdpSocket;
use std::sync::Arc;
use std::sync::mpsc::sync_channel;
use tcp_listener::*;
use udp_listener::*;
use varz::*;

#[cfg(feature = "webservice")]
use webservice::*;

const DNS_MAX_SIZE: usize = 65535;
const DNS_MAX_TCP_SIZE: usize = 65535;
const DNS_MAX_UDP_SIZE: usize = 4096;
const DNS_QUERY_MAX_SIZE: usize = 283;
const DNS_QUERY_MIN_SIZE: usize = 17;
const DNS_UDP_NOEDNS0_MAX_SIZE: usize = 512;
const HEALTH_CHECK_MS: u64 = 10 * 1000;
const MAX_ACTIVE_QUERIES: usize = 100_000;
const MAX_CLIENTS_WAITING_FOR_QUERY: usize = 1_000;
const MAX_EVENTS_PER_BATCH: usize = 1024;
const MAX_TCP_CLIENTS: usize = 1_000;
const MAX_TCP_HASH_DISTANCE: usize = 10;
const MAX_TCP_IDLE_MS: u64 = 10 * 1000;
const MAX_WAITING_CLIENTS: usize = MAX_ACTIVE_QUERIES * 10;
const FAILURE_TTL: u32 = 30;
const UDP_BUFFER_SIZE: usize = 16 * 1024 * 1024;
const UPSTREAM_INITIAL_TIMEOUT_MS: u64 = 1 * 1000;
const UPSTREAM_MAX_TIMEOUT_MS: u64 = 8 * 1000;
const UPSTREAM_TIMEOUT_MS: u64 = 10 * 1000;

#[cfg(feature = "webservice")]
const WEBSERVICE_THREADS: usize = 1;

pub struct RPDNSContext {
    pub config: Config,
    pub udp_socket: UdpSocket,
    pub listen_addr: String,
    pub cache: Cache,
    pub varz: Arc<Varz>,
}

struct RPDNS;

impl RPDNS {
    #[cfg(feature = "webservice")]
    fn webservice_start(rpdns_context: &RPDNSContext) {
        WebService::spawn(rpdns_context).expect("Unable to spawn the web service");
    }

    #[cfg(not(feature = "webservice"))]
    fn webservice_start(_rpdns_context: &RPDNSContext) {}

    fn privileges_drop(config: &Config) {
        let mut pd = PrivDrop::default();
        if let Some(ref user) = config.user {
            pd = pd.user(user);
        }
        if let Some(ref group) = config.group {
            pd = pd.group(group);
        }
        if let Some(ref chroot_dir) = config.chroot_dir {
            pd = pd.chroot(chroot_dir);
        }
        pd.apply().unwrap();
    }

    fn new(config: Config) -> RPDNS {
        let varz = Arc::new(Varz::new());
        let cache = Cache::new(config.clone());
        let udp_socket = socket_udp_bound(&config.listen_addr)
            .expect("Unable to create a client socket");
        let rpdns_context = RPDNSContext {
            config: config.clone(),
            udp_socket: udp_socket,
            listen_addr: config.listen_addr.to_owned(),
            cache: cache,
            varz: varz,
        };
        let resolver_tx = Resolver::spawn(&rpdns_context).expect("Unable to spawn the resolver");
        if config.webservice_enabled {
            Self::webservice_start(&rpdns_context);
        }
        let (service_ready_tx, service_ready_rx) = sync_channel::<u8>(1);
        let udp_listener = UdpListener::spawn(&rpdns_context,
                                              resolver_tx.clone(),
                                              service_ready_tx.clone())
            .expect("Unable to spawn a UDP listener");
        service_ready_rx.recv().unwrap();
        let tcp_listener = TcpListener::spawn(&rpdns_context,
                                              resolver_tx.clone(),
                                              service_ready_tx.clone())
            .expect("Unable to spawn a TCP listener");
        service_ready_rx.recv().unwrap();
        Self::privileges_drop(&config);
        info!("EdgeDNS is ready to process requests");
        let _ = udp_listener.join();
        let _ = tcp_listener.join();

        RPDNS
    }
}

fn main() {
    env_logger::init().expect("Failed to init logger");

    let matches = App::new("EdgeDNS")
        .version("0.2.0")
        .author("Frank Denis")
        .about("A caching DNS reverse proxy")
        .arg(Arg::with_name("config_file")
            .short("c")
            .long("config")
            .value_name("FILE")
            .help("Path to the edgedns.toml config file")
            .takes_value(true)
            .required(true))
        .get_matches();

    let config_file = match matches.value_of("config_file") {
        None => {
            error!("A path to the configuration file is required");
            return;
        }
        Some(config_file) => config_file,
    };
    let config = match Config::from_path(config_file) {
        Err(err) => {
            error!("The configuration couldn't be loaded -- [{}]: [{}]",
                   config_file,
                   err);
            return;
        }
        Ok(config) => config,
    };
    RPDNS::new(config);
}
