use std::collections::BTreeMap;

use anyhow::{bail, Context};

use sozu_command_lib::{
    certificate::{
        decode_fingerprint, get_fingerprint_from_certificate_path, load_full_certificate,
    },
    config::ListenerBuilder,
    proto::command::{
        request::RequestType, ActivateListener, AddBackend, AddCertificate, Cluster, CountRequests,
        DeactivateListener, FrontendFilters, HardStop, ListListeners, ListenerType,
        LoadBalancingParams, MetricsConfiguration, PathRule, ProxyProtocolConfig,
        QueryCertificatesFilters, QueryClusterByDomain, QueryClustersHashes, RemoveBackend,
        RemoveCertificate, RemoveListener, ReplaceCertificate, RequestHttpFrontend,
        RequestTcpFrontend, RulePosition, SoftStop, Status, SubscribeEvents, TlsVersion,
    },
};

use crate::{
    cli::{
        BackendCmd, ClusterCmd, HttpFrontendCmd, HttpListenerCmd, HttpsListenerCmd, LoggingLevel,
        MetricsCmd, TcpFrontendCmd, TcpListenerCmd,
    },
    ctl::CommandManager,
};

impl CommandManager {
    pub fn save_state(&mut self, path: String) -> anyhow::Result<()> {
        debug!("Saving the state to file {}", path);

        self.send_request(RequestType::SaveState(path).into())
    }

    pub fn load_state(&mut self, path: String) -> anyhow::Result<()> {
        debug!("Loading the state on path {}", path);

        self.send_request(RequestType::LoadState(path).into())
    }

    pub fn count_requests(&mut self) -> anyhow::Result<()> {
        self.send_request(RequestType::CountRequests(CountRequests {}).into())
    }

    pub fn soft_stop(&mut self) -> anyhow::Result<()> {
        debug!("shutting down proxy softly");

        self.send_request(RequestType::SoftStop(SoftStop {}).into())
    }

    pub fn hard_stop(&mut self) -> anyhow::Result<()> {
        debug!("shutting down proxy the hard way");

        self.send_request(RequestType::HardStop(HardStop {}).into())
    }

    pub fn status(&mut self) -> anyhow::Result<()> {
        debug!("Requesting status…");

        self.send_request(RequestType::Status(Status {}).into())
    }

    pub fn configure_metrics(&mut self, cmd: MetricsCmd) -> anyhow::Result<()> {
        debug!("Configuring metrics: {:?}", cmd);

        let configuration = match cmd {
            MetricsCmd::Enable => MetricsConfiguration::Enabled,
            MetricsCmd::Disable => MetricsConfiguration::Disabled,
            MetricsCmd::Clear => MetricsConfiguration::Clear,
            _ => bail!("The command passed to the configure_metrics function is wrong."),
        };

        self.send_request(RequestType::ConfigureMetrics(configuration as i32).into())
    }

    pub fn reload_configuration(&mut self, path: Option<String>) -> anyhow::Result<()> {
        debug!("Reloading configuration…");
        let path = match path {
            Some(p) => p,
            None => String::new(),
        };
        self.send_request(RequestType::ReloadConfiguration(path).into())
    }

    pub fn list_frontends(
        &mut self,
        http: bool,
        https: bool,
        tcp: bool,
        domain: Option<String>,
    ) -> anyhow::Result<()> {
        debug!("Listing frontends");

        self.send_request(
            RequestType::ListFrontends(FrontendFilters {
                http,
                https,
                tcp,
                domain,
            })
            .into(),
        )
    }

    pub fn events(&mut self) -> anyhow::Result<()> {
        self.send_request(RequestType::SubscribeEvents(SubscribeEvents {}).into())
    }

    pub fn backend_command(&mut self, cmd: BackendCmd) -> anyhow::Result<()> {
        match cmd {
            BackendCmd::Add {
                id,
                backend_id,
                address,
                sticky_id,
                backup,
            } => self.send_request(
                RequestType::AddBackend(AddBackend {
                    cluster_id: id,
                    address: address.to_string(),
                    backend_id,
                    load_balancing_parameters: Some(LoadBalancingParams::default()),
                    sticky_id,
                    backup,
                })
                .into(),
            ),
            BackendCmd::Remove {
                id,
                backend_id,
                address,
            } => self.send_request(
                RequestType::RemoveBackend(RemoveBackend {
                    cluster_id: id,
                    address: address.to_string(),
                    backend_id,
                })
                .into(),
            ),
        }
    }

    pub fn cluster_command(&mut self, cmd: ClusterCmd) -> anyhow::Result<()> {
        match cmd {
            ClusterCmd::Add {
                id,
                sticky_session,
                https_redirect,
                send_proxy,
                expect_proxy,
                load_balancing_policy,
            } => {
                let proxy_protocol = match (send_proxy, expect_proxy) {
                    (true, true) => Some(ProxyProtocolConfig::RelayHeader),
                    (true, false) => Some(ProxyProtocolConfig::SendHeader),
                    (false, true) => Some(ProxyProtocolConfig::ExpectHeader),
                    _ => None,
                };
                self.send_request(
                    RequestType::AddCluster(Cluster {
                        cluster_id: id,
                        sticky_session,
                        https_redirect,
                        proxy_protocol: proxy_protocol.map(|pp| pp as i32),
                        load_balancing: load_balancing_policy as i32,
                        ..Default::default()
                    })
                    .into(),
                )
            }
            ClusterCmd::Remove { id } => self.send_request(RequestType::RemoveCluster(id).into()),
            ClusterCmd::List {
                id: cluster_id,
                domain,
            } => {
                if cluster_id.is_some() && domain.is_some() {
                    bail!("Error: Either request an cluster ID or a domain name");
                }

                let request = if let Some(ref cluster_id) = cluster_id {
                    RequestType::QueryClusterById(cluster_id.to_string()).into()
                } else if let Some(ref domain) = domain {
                    let splitted: Vec<String> =
                        domain.splitn(2, '/').map(|elem| elem.to_string()).collect();

                    if splitted.is_empty() {
                        bail!("Domain can't be empty");
                    }

                    let query_domain = QueryClusterByDomain {
                        hostname: splitted
                            .get(0)
                            .with_context(|| "Domain can't be empty")?
                            .clone(),
                        path: splitted.get(1).cloned().map(|path| format!("/{path}")), // We add the / again because of the splitn removing it
                    };

                    RequestType::QueryClustersByDomain(query_domain).into()
                } else {
                    RequestType::QueryClustersHashes(QueryClustersHashes {}).into()
                };

                self.send_request(request)
            }
        }
    }

    pub fn tcp_frontend_command(&mut self, cmd: TcpFrontendCmd) -> anyhow::Result<()> {
        match cmd {
            TcpFrontendCmd::Add { id, address, tags } => self.send_request(
                RequestType::AddTcpFrontend(RequestTcpFrontend {
                    cluster_id: id,
                    address: address.to_string(),
                    tags: tags.unwrap_or(BTreeMap::new()),
                })
                .into(),
            ),
            TcpFrontendCmd::Remove { id, address } => self.send_request(
                RequestType::RemoveTcpFrontend(RequestTcpFrontend {
                    cluster_id: id,
                    address: address.to_string(),
                    ..Default::default()
                })
                .into(),
            ),
        }
    }

    pub fn http_frontend_command(&mut self, cmd: HttpFrontendCmd) -> anyhow::Result<()> {
        match cmd {
            HttpFrontendCmd::Add {
                hostname,
                path_prefix,
                path_regex,
                path_equals,
                address,
                method,
                cluster_id: route,
                tags,
            } => self.send_request(
                RequestType::AddHttpFrontend(RequestHttpFrontend {
                    cluster_id: route.into(),
                    address: address.to_string(),
                    hostname,
                    path: PathRule::from_cli_options(path_prefix, path_regex, path_equals),
                    method: method.map(String::from),
                    position: RulePosition::Tree.into(),
                    tags: match tags {
                        Some(tags) => tags,
                        None => BTreeMap::new(),
                    },
                })
                .into(),
            ),
            HttpFrontendCmd::Remove {
                hostname,
                path_prefix,
                path_regex,
                path_equals,
                address,
                method,
                cluster_id: route,
            } => self.send_request(
                RequestType::RemoveHttpFrontend(RequestHttpFrontend {
                    cluster_id: route.into(),
                    address: address.to_string(),
                    hostname,
                    path: PathRule::from_cli_options(path_prefix, path_regex, path_equals),
                    method: method.map(String::from),
                    ..Default::default()
                })
                .into(),
            ),
        }
    }

    pub fn https_frontend_command(&mut self, cmd: HttpFrontendCmd) -> anyhow::Result<()> {
        match cmd {
            HttpFrontendCmd::Add {
                hostname,
                path_prefix,
                path_regex,
                path_equals,
                address,
                method,
                cluster_id: route,
                tags,
            } => self.send_request(
                RequestType::AddHttpsFrontend(RequestHttpFrontend {
                    cluster_id: route.into(),
                    address: address.to_string(),
                    hostname,
                    path: PathRule::from_cli_options(path_prefix, path_regex, path_equals),
                    method: method.map(String::from),
                    position: RulePosition::Tree.into(),
                    tags: match tags {
                        Some(tags) => tags,
                        None => BTreeMap::new(),
                    },
                })
                .into(),
            ),
            HttpFrontendCmd::Remove {
                hostname,
                path_prefix,
                path_regex,
                path_equals,
                address,
                method,
                cluster_id: route,
            } => self.send_request(
                RequestType::RemoveHttpsFrontend(RequestHttpFrontend {
                    cluster_id: route.into(),
                    address: address.to_string(),
                    hostname,
                    path: PathRule::from_cli_options(path_prefix, path_regex, path_equals),
                    method: method.map(String::from),
                    ..Default::default()
                })
                .into(),
            ),
        }
    }

    pub fn https_listener_command(&mut self, cmd: HttpsListenerCmd) -> anyhow::Result<()> {
        match cmd {
            HttpsListenerCmd::Add {
                address,
                public_address,
                answer_404,
                answer_503,
                tls_versions,
                cipher_list,
                expect_proxy,
                sticky_name,
                front_timeout,
                back_timeout,
                request_timeout,
                connect_timeout,
            } => {
                let https_listener = ListenerBuilder::new_https(address)
                    .with_public_address(public_address)
                    .with_answer_404_path(answer_404)
                    .with_answer_503_path(answer_503)
                    .with_tls_versions(tls_versions)
                    .with_cipher_list(cipher_list)
                    .with_expect_proxy(expect_proxy)
                    .with_sticky_name(sticky_name)
                    .with_front_timeout(front_timeout)
                    .with_back_timeout(back_timeout)
                    .with_request_timeout(request_timeout)
                    .with_connect_timeout(connect_timeout)
                    .to_tls(Some(&self.config))
                    .with_context(|| "Error creating HTTPS listener")?;

                self.send_request(RequestType::AddHttpsListener(https_listener).into())
            }
            HttpsListenerCmd::Remove { address } => {
                self.remove_listener(address.to_string(), ListenerType::Https)
            }
            HttpsListenerCmd::Activate { address } => {
                self.activate_listener(address.to_string(), ListenerType::Https)
            }
            HttpsListenerCmd::Deactivate { address } => {
                self.deactivate_listener(address.to_string(), ListenerType::Https)
            }
        }
    }

    pub fn http_listener_command(&mut self, cmd: HttpListenerCmd) -> anyhow::Result<()> {
        match cmd {
            HttpListenerCmd::Add {
                address,
                public_address,
                answer_404,
                answer_503,
                expect_proxy,
                sticky_name,
                front_timeout,
                back_timeout,
                request_timeout,
                connect_timeout,
            } => {
                let http_listener = ListenerBuilder::new_http(address)
                    .with_public_address(public_address)
                    .with_answer_404_path(answer_404)
                    .with_answer_503_path(answer_503)
                    .with_expect_proxy(expect_proxy)
                    .with_sticky_name(sticky_name)
                    .with_front_timeout(front_timeout)
                    .with_request_timeout(request_timeout)
                    .with_back_timeout(back_timeout)
                    .with_connect_timeout(connect_timeout)
                    .to_http(Some(&self.config))
                    .with_context(|| "Error creating HTTP listener")?;
                self.send_request(RequestType::AddHttpListener(http_listener).into())
            }
            HttpListenerCmd::Remove { address } => {
                self.remove_listener(address.to_string(), ListenerType::Http)
            }
            HttpListenerCmd::Activate { address } => {
                self.activate_listener(address.to_string(), ListenerType::Http)
            }
            HttpListenerCmd::Deactivate { address } => {
                self.deactivate_listener(address.to_string(), ListenerType::Http)
            }
        }
    }

    pub fn tcp_listener_command(&mut self, cmd: TcpListenerCmd) -> anyhow::Result<()> {
        match cmd {
            TcpListenerCmd::Add {
                address,
                public_address,
                expect_proxy,
            } => {
                let listener = ListenerBuilder::new_tcp(address)
                    .with_public_address(public_address)
                    .with_expect_proxy(expect_proxy)
                    .to_tcp(Some(&self.config))
                    .with_context(|| "Could not create TCP listener")?;

                self.send_request(RequestType::AddTcpListener(listener).into())
            }
            TcpListenerCmd::Remove { address } => {
                self.remove_listener(address.to_string(), ListenerType::Tcp)
            }
            TcpListenerCmd::Activate { address } => {
                self.activate_listener(address.to_string(), ListenerType::Tcp)
            }
            TcpListenerCmd::Deactivate { address } => {
                self.deactivate_listener(address.to_string(), ListenerType::Tcp)
            }
        }
    }

    pub fn list_listeners(&mut self) -> anyhow::Result<()> {
        self.send_request(RequestType::ListListeners(ListListeners {}).into())
    }

    pub fn remove_listener(
        &mut self,
        address: String,
        listener_type: ListenerType,
    ) -> anyhow::Result<()> {
        self.send_request(
            RequestType::RemoveListener(RemoveListener {
                address: address.parse().with_context(|| "wrong socket address")?,
                proxy: listener_type.into(),
            })
            .into(),
        )
    }

    pub fn activate_listener(
        &mut self,
        address: String,
        listener_type: ListenerType,
    ) -> anyhow::Result<()> {
        self.send_request(
            RequestType::ActivateListener(ActivateListener {
                address: address.parse().with_context(|| "wrong socket address")?,
                proxy: listener_type.into(),
                from_scm: false,
            })
            .into(),
        )
    }

    pub fn deactivate_listener(
        &mut self,
        address: String,
        listener_type: ListenerType,
    ) -> anyhow::Result<()> {
        self.send_request(
            RequestType::DeactivateListener(DeactivateListener {
                address: address.parse().with_context(|| "wrong socket address")?,
                proxy: listener_type.into(),
                to_scm: false,
            })
            .into(),
        )
    }

    pub fn logging_filter(&mut self, filter: &LoggingLevel) -> anyhow::Result<()> {
        self.send_request(RequestType::Logging(filter.to_string().to_lowercase()).into())
    }

    pub fn add_certificate(
        &mut self,
        address: String,
        certificate_path: &str,
        certificate_chain_path: &str,
        key_path: &str,
        versions: Vec<TlsVersion>,
    ) -> anyhow::Result<()> {
        let new_certificate = load_full_certificate(
            certificate_path,
            certificate_chain_path,
            key_path,
            versions,
            vec![],
        )
        .with_context(|| "Could not load the full certificate")?;

        self.send_request(
            RequestType::AddCertificate(AddCertificate {
                address,
                certificate: new_certificate,
                expired_at: None,
            })
            .into(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn replace_certificate(
        &mut self,
        address: String,
        new_certificate_path: &str,
        new_certificate_chain_path: &str,
        new_key_path: &str,
        old_certificate_path: Option<&str>,
        old_fingerprint: Option<&str>,
        versions: Vec<TlsVersion>,
    ) -> anyhow::Result<()> {
        let old_fingerprint = match (old_certificate_path, old_fingerprint) {
            (None, None) | (Some(_), Some(_)) => {
                bail!("Error: Please provide either one, the old certificate's path OR its fingerprint")
            }
            (Some(old_certificate_path), None) => {
                get_fingerprint_from_certificate_path(old_certificate_path).with_context(|| {
                    "Could not retrieve the fingerprint from the given certificate path"
                })?
            }
            (None, Some(fingerprint)) => decode_fingerprint(fingerprint)
                .with_context(|| "Error decoding the given fingerprint")?,
        };

        let new_certificate = load_full_certificate(
            new_certificate_path,
            new_certificate_chain_path,
            new_key_path,
            versions,
            vec![],
        )
        .with_context(|| "Could not load the full certificate")?;

        self.send_request(
            RequestType::ReplaceCertificate(ReplaceCertificate {
                address,
                new_certificate,
                old_fingerprint: old_fingerprint.to_string(),
                new_expired_at: None,
            })
            .into(),
        )?;

        Ok(())
    }

    pub fn remove_certificate(
        &mut self,
        address: String,
        certificate_path: Option<&str>,
        fingerprint: Option<&str>,
    ) -> anyhow::Result<()> {
        let fingerprint = match (certificate_path, fingerprint) {
            (None, None) | (Some(_), Some(_)) => {
                bail!("Error: Please provide either one, the path OR the fingerprint of the certificate")
            }
            (Some(certificate_path), None) => {
                get_fingerprint_from_certificate_path(certificate_path).with_context(|| {
                    "Could not retrieve the finger print from the given certificate path"
                })?
            }
            (None, Some(fingerprint)) => decode_fingerprint(fingerprint)
                .with_context(|| "Error decoding the given fingerprint")?,
        };

        self.send_request(
            RequestType::RemoveCertificate(RemoveCertificate {
                address,
                fingerprint: fingerprint.to_string(),
            })
            .into(),
        )
    }

    pub fn query_certificates(
        &mut self,
        fingerprint: Option<String>,
        domain: Option<String>,
        query_workers: bool,
    ) -> Result<(), anyhow::Error> {
        let filters = QueryCertificatesFilters {
            domain,
            fingerprint,
        };

        if query_workers {
            self.send_request(RequestType::QueryCertificatesFromWorkers(filters).into())
        } else {
            self.send_request(RequestType::QueryCertificatesFromTheState(filters).into())
        }
    }
}
