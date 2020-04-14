use std::collections::HashMap;
use std::env;
use std::str::FromStr;
use std::sync::Arc;

use futures::sync::mpsc::Sender;
use lazy_static::lazy_static;

use graph::prelude::{
    format_err, BlockState, CheapClone, DataSource, DataSourceTemplate, Error, EthereumCallCache,
    EthereumTrigger, HostMetrics, LightEthereumBlock, LightEthereumBlockExt, Logger, Store,
    SubgraphDeploymentId, SubgraphDeploymentStore, SubgraphManifest,
};
use graph_runtime_wasm::{MappingRequest, RuntimeHost, RuntimeHostBuilder};

lazy_static! {
    static ref MAX_DATA_SOURCES: Option<usize> = env::var("GRAPH_SUBGRAPH_MAX_DATA_SOURCES")
        .ok()
        .map(|s| usize::from_str(&s)
            .unwrap_or_else(|_| panic!("failed to parse env var GRAPH_SUBGRAPH_MAX_DATA_SOURCES")));
}

pub struct SubgraphInstance<S> {
    subgraph_id: SubgraphDeploymentId,
    network: String,
    host_builder: RuntimeHostBuilder<S>,

    /// Runtime hosts, one for each data source mapping.
    ///
    /// The runtime hosts are created and added in the same order the
    /// data sources appear in the subgraph manifest. Incoming block
    /// stream events are processed by the mappings in this same order.
    hosts: Vec<Arc<RuntimeHost>>,

    /// Maps a serialized module to a channel to the thread in which the module is instantiated.
    module_cache: HashMap<Vec<u8>, Sender<MappingRequest>>,
}

impl<S> SubgraphInstance<S>
where
    S: Store + SubgraphDeploymentStore + EthereumCallCache,
{
    pub(crate) fn from_manifest(
        logger: &Logger,
        manifest: SubgraphManifest,
        host_builder: RuntimeHostBuilder<S>,
        host_metrics: Arc<HostMetrics>,
    ) -> Result<Self, Error> {
        let subgraph_id = manifest.id.clone();
        let network = manifest.network_name();
        let templates = Arc::new(manifest.templates);

        let mut this = SubgraphInstance {
            host_builder,
            subgraph_id,
            network,
            hosts: Vec::new(),
            module_cache: HashMap::new(),
        };

        // Create a new runtime host for each data source in the subgraph manifest;
        // we use the same order here as in the subgraph manifest to make the
        // event processing behavior predictable
        let (hosts, errors): (_, Vec<_>) = manifest
            .data_sources
            .into_iter()
            .map(|d| this.new_host(logger.clone(), d, templates.clone(), host_metrics.clone()))
            .partition(|res| res.is_ok());

        if !errors.is_empty() {
            let joined_errors = errors
                .into_iter()
                .map(Result::unwrap_err)
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(format_err!(
                "Errors loading data sources: {}",
                joined_errors
            ));
        }

        this.hosts = hosts
            .into_iter()
            .map(Result::unwrap)
            .map(Arc::new)
            .collect();

        Ok(this)
    }

    fn new_host(
        &mut self,
        logger: Logger,
        data_source: DataSource,
        top_level_templates: Arc<Vec<DataSourceTemplate>>,
        host_metrics: Arc<HostMetrics>,
    ) -> Result<RuntimeHost, Error> {
        let mapping_request_sender = {
            let module_bytes = data_source.mapping.runtime.as_ref().clone().to_bytes()?;
            if let Some(sender) = self.module_cache.get(&module_bytes) {
                sender.clone()
            } else {
                let sender = RuntimeHostBuilder::<S>::spawn_mapping(
                    data_source.mapping.runtime.as_ref().clone(),
                    logger,
                    self.subgraph_id.clone(),
                    Arc::new(vec![]),
                    host_metrics.clone(),
                )?;
                self.module_cache.insert(module_bytes, sender.clone());
                sender
            }
        };
        self.host_builder.build(
            self.network.clone(),
            self.subgraph_id.clone(),
            data_source,
            top_level_templates,
            mapping_request_sender,
            host_metrics,
        )
    }

    pub async fn process_trigger(
        &self,
        logger: &Logger,
        block: &Arc<LightEthereumBlock>,
        trigger: EthereumTrigger,
        state: BlockState,
    ) -> Result<BlockState, Error> {
        Self::process_trigger_in_runtime_hosts(logger, &self.hosts, block, trigger, state).await
    }

    pub async fn process_trigger_in_runtime_hosts(
        logger: &Logger,
        hosts: &[Arc<RuntimeHost>],
        block: &Arc<LightEthereumBlock>,
        trigger: EthereumTrigger,
        mut state: BlockState,
    ) -> Result<BlockState, Error> {
        match trigger {
            EthereumTrigger::Log(log) => {
                let log = Arc::new(log);

                let transaction = block
                    .transaction_for_log(&log)
                    .map(Arc::new)
                    .ok_or_else(|| format_err!("Found no transaction for event"))?;
                let matching_hosts = hosts.iter().filter(|host| host.matches_log(&log));
                // Process the log in each host in the same order the corresponding data
                // sources appear in the subgraph manifest
                let transaction = Arc::new(transaction);
                for host in matching_hosts {
                    state = host
                        .process_log(logger, block, &transaction, &log, state)
                        .await?;
                }
            }
            EthereumTrigger::Call(call) => {
                let call = Arc::new(call);

                let transaction = block
                    .transaction_for_call(&call)
                    .ok_or_else(|| format_err!("Found no transaction for call"))?;
                let transaction = Arc::new(transaction);
                let matching_hosts = hosts.iter().filter(|host| host.matches_call(&call));

                for host in matching_hosts {
                    state = host
                        .process_call(logger, block, &transaction, &call, state)
                        .await?;
                }
            }
            EthereumTrigger::Block(ptr, trigger_type) => {
                let matching_hosts = hosts
                    .iter()
                    .filter(|host| host.matches_block(&trigger_type, ptr.number));
                for host in matching_hosts {
                    state = host
                        .process_block(logger, block, &trigger_type, state)
                        .await?;
                }
            }
        }
        Ok(state)
    }

    pub fn add_dynamic_data_source(
        &mut self,
        logger: &Logger,
        data_source: DataSource,
        top_level_templates: Arc<Vec<DataSourceTemplate>>,
        metrics: Arc<HostMetrics>,
    ) -> Result<Arc<RuntimeHost>, Error> {
        // Protect against creating more than the allowed maximum number of data sources
        if let Some(max_data_sources) = *MAX_DATA_SOURCES {
            if self.hosts.len() >= max_data_sources {
                return Err(format_err!(
                    "Limit of {} data sources per subgraph exceeded",
                    max_data_sources
                ));
            }
        }

        let host = Arc::new(self.new_host(
            logger.clone(),
            data_source,
            top_level_templates,
            metrics.clone(),
        )?);
        self.hosts.push(host.cheap_clone());
        Ok(host)
    }
}
