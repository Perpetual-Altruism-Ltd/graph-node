use std::collections::HashMap;
use std::ops::Deref;
use std::str::FromStr;
use std::time::{Duration, Instant, SystemTime};
use std::env;

use never::Never;
use semver::Version;
use wasmtime::Trap;
use web3::types::H160;

use graph::blockchain::DataSource;
use graph::blockchain::{Blockchain, DataSourceTemplate as _};
use graph::components::store::EntityType;
use graph::components::store::{EnsLookup, EntityKey};
use graph::components::subgraph::{CausalityRegion, ProofOfIndexingEvent, SharedProofOfIndexing};
use graph::data::store;
use graph::ensure;
use graph::prelude::ethabi::param_type::Reader;
use graph::prelude::ethabi::{decode, encode, Token};
use graph::prelude::{serde_json};
use graph::prelude::{slog::b, slog::record_static, *};
use graph::runtime::gas::{self, complexity, Gas, GasCounter};
pub use graph::runtime::{DeterministicHostError, HostExportError};

use crate::module::{WasmInstance, WasmInstanceContext};
use crate::{error::DeterminismLevel, module::IntoTrap};

use rdkafka::{
    config::ClientConfig,
    producer::future_producer::{
        FutureProducer,
        FutureRecord,
    },
};

use common::events::{
    apple_notification::{AppleNotification, ApnsHeaders, AppleLocalizedAlert, CustomData},
    webpush_notification::WebPushNotification,
    rpc::Request,
    push_notification::PushNotification,
    google_notification::{GoogleNotification, GoogleLocalizedAlert, GoogleMessage, GoogleNotification_Priority},
    http_request::{HttpRequest, HttpRequest_HttpVerb::POST},
};

use protobuf::Message;


#[derive(Serialize, Debug)]
#[allow(non_camel_case_types)]
struct rpc_call {
    method: String,
    params: Vec<String>,
    id: String,
    jsonrpc: String
}



fn write_poi_event(
    proof_of_indexing: &SharedProofOfIndexing,
    poi_event: &ProofOfIndexingEvent,
    causality_region: &str,
    logger: &Logger,
) {
    if let Some(proof_of_indexing) = proof_of_indexing {
        let mut proof_of_indexing = proof_of_indexing.deref().borrow_mut();
        proof_of_indexing.write(logger, causality_region, poi_event);
    }
}

impl IntoTrap for HostExportError {
    fn determinism_level(&self) -> DeterminismLevel {
        match self {
            HostExportError::Deterministic(_) => DeterminismLevel::Deterministic,
            HostExportError::Unknown(_) => DeterminismLevel::Unimplemented,
            HostExportError::PossibleReorg(_) => DeterminismLevel::PossibleReorg,
        }
    }
    fn into_trap(self) -> Trap {
        match self {
            HostExportError::Unknown(e)
            | HostExportError::PossibleReorg(e)
            | HostExportError::Deterministic(e) => Trap::from(e),
        }
    }
}

pub struct HostExports<C: Blockchain> {
    pub(crate) subgraph_id: DeploymentHash,
    pub api_version: Version,
    data_source_name: String,
    data_source_address: Vec<u8>,
    data_source_network: String,
    data_source_context: Arc<Option<DataSourceContext>>,
    /// Some data sources have indeterminism or different notions of time. These
    /// need to be each be stored separately to separate causality between them,
    /// and merge the results later. Right now, this is just the ethereum
    /// networks but will be expanded for ipfs and the availability chain.
    causality_region: String,
    templates: Arc<Vec<C::DataSourceTemplate>>,
    pub(crate) link_resolver: Arc<dyn LinkResolver>,
    ens_lookup: Arc<dyn EnsLookup>,
    producer: FutureProducer,
    auth: Option<String>,
    p256dh: Option<String>,
    rpc_id: String,
}

impl<C: Blockchain> HostExports<C> {
    pub fn new(
        subgraph_id: DeploymentHash,
        data_source: &impl DataSource<C>,
        data_source_network: String,
        templates: Arc<Vec<C::DataSourceTemplate>>,
        link_resolver: Arc<dyn LinkResolver>,
        ens_lookup: Arc<dyn EnsLookup>,
    ) -> Self {
        //let kafka_server = &env::var("KAFKA_SERVER").expect("Did not find KAFKA_SERVER");

        let kaf_producer: FutureProducer = ClientConfig::new()
            .set("bootstrap.servers", "localhost:9092")
            .set("produce.offset.report", "true")
            .create()
            .expect("Producer creation error");

        let au = match env::var("WEBPUSH_AUTH"){
            Ok(t) => Some(t),
            Err(_) => None,
        };

        let pdh = match env::var("WEBPUSH_P256DH"){
            Ok(t) => Some(t),
            Err(_) => None,
        };

        let rpc_id = match env::var("RPC_ID") {
            Ok(t) => t,
            Err(_) => String::from("null"),
        };



        Self {
            subgraph_id,
            api_version: data_source.api_version(),
            data_source_name: data_source.name().to_owned(),
            data_source_address: data_source.address().unwrap_or_default().to_owned(),
            data_source_context: data_source.context().cheap_clone(),
            causality_region: CausalityRegion::from_network(&data_source_network),
            data_source_network,
            templates,
            link_resolver,
            ens_lookup,
            producer: kaf_producer,
            auth: au,
            p256dh: pdh,
            rpc_id
        }
    }

    pub(crate) fn abort(
        &self,
        message: Option<String>,
        file_name: Option<String>,
        line_number: Option<u32>,
        column_number: Option<u32>,
        gas: &GasCounter,
    ) -> Result<Never, DeterministicHostError> {
        gas.consume_host_fn(Gas::new(gas::DEFAULT_BASE_COST))?;

        let message = message
            .map(|message| format!("message: {}", message))
            .unwrap_or_else(|| "no message".into());
        let location = match (file_name, line_number, column_number) {
            (None, None, None) => "an unknown location".into(),
            (Some(file_name), None, None) => file_name,
            (Some(file_name), Some(line_number), None) => {
                format!("{}, line {}", file_name, line_number)
            }
            (Some(file_name), Some(line_number), Some(column_number)) => format!(
                "{}, line {}, column {}",
                file_name, line_number, column_number
            ),
            _ => unreachable!(),
        };
        Err(DeterministicHostError::from(anyhow::anyhow!(
            "Mapping aborted at {}, with {}",
            location,
            message
        )))
    }

    pub(crate) fn store_set(
        &self,
        logger: &Logger,
        state: &mut BlockState<C>,
        proof_of_indexing: &SharedProofOfIndexing,
        entity_type: String,
        entity_id: String,
        data: HashMap<String, Value>,
        stopwatch: &StopwatchMetrics,
        gas: &GasCounter,
    ) -> Result<(), anyhow::Error> {
        let poi_section = stopwatch.start_section("host_export_store_set__proof_of_indexing");
        write_poi_event(
            proof_of_indexing,
            &ProofOfIndexingEvent::SetEntity {
                entity_type: &entity_type,
                id: &entity_id,
                data: &data,
            },
            &self.causality_region,
            logger,
        );
        poi_section.end();

        let key = EntityKey {
            subgraph_id: self.subgraph_id.clone(),
            entity_type: EntityType::new(entity_type),
            entity_id,
        };

        gas.consume_host_fn(gas::STORE_SET.with_args(complexity::Linear, (&key, &data)))?;

        let entity = Entity::from(data);
        state.entity_cache.set(key.clone(), entity)?;

        Ok(())
    }

    pub(crate) fn store_remove(
        &self,
        logger: &Logger,
        state: &mut BlockState<C>,
        proof_of_indexing: &SharedProofOfIndexing,
        entity_type: String,
        entity_id: String,
        gas: &GasCounter,
    ) -> Result<(), HostExportError> {
        write_poi_event(
            proof_of_indexing,
            &ProofOfIndexingEvent::RemoveEntity {
                entity_type: &entity_type,
                id: &entity_id,
            },
            &self.causality_region,
            logger,
        );
        let key = EntityKey {
            subgraph_id: self.subgraph_id.clone(),
            entity_type: EntityType::new(entity_type),
            entity_id,
        };

        gas.consume_host_fn(gas::STORE_REMOVE.with_args(complexity::Size, &key))?;

        state.entity_cache.remove(key);

        Ok(())
    }

    pub(crate) fn store_get(
        &self,
        state: &mut BlockState<C>,
        entity_type: String,
        entity_id: String,
        gas: &GasCounter,
    ) -> Result<Option<Entity>, anyhow::Error> {
        let store_key = EntityKey {
            subgraph_id: self.subgraph_id.clone(),
            entity_type: EntityType::new(entity_type),
            entity_id,
        };

        let result = state.entity_cache.get(&store_key)?;
        gas.consume_host_fn(gas::STORE_GET.with_args(complexity::Linear, (&store_key, &result)))?;

        Ok(result)
    }

    /// Prints the module of `n` in hex.
    /// Integers are encoded using the least amount of digits (no leading zero digits).
    /// Their encoding may be of uneven length. The number zero encodes as "0x0".
    ///
    /// https://godoc.org/github.com/ethereum/go-ethereum/common/hexutil#hdr-Encoding_Rules
    pub(crate) fn big_int_to_hex(
        &self,
        n: BigInt,
        gas: &GasCounter,
    ) -> Result<String, DeterministicHostError> {
        gas.consume_host_fn(gas::DEFAULT_GAS_OP.with_args(complexity::Size, &n))?;

        if n == 0.into() {
            return Ok("0x0".to_string());
        }

        let bytes = n.to_bytes_be().1;
        Ok(format!(
            "0x{}",
            ::hex::encode(bytes).trim_start_matches('0')
        ))
    }

    pub(crate) fn ipfs_cat(&self, logger: &Logger, link: String) -> Result<Vec<u8>, anyhow::Error> {
        // Does not consume gas because this is not a part of the deterministic feature set.
        // Ideally this would first consume gas for fetching the file stats, and then again
        // for the bytes of the file.
        graph::block_on(self.link_resolver.cat(logger, &Link { link }))
    }

    pub(crate) fn ipfs_get_block(
        &self,
        logger: &Logger,
        link: String,
    ) -> Result<Vec<u8>, anyhow::Error> {
        // Does not consume gas because this is not a part of the deterministic feature set.
        // Ideally this would first consume gas for fetching the file stats, and then again
        // for the bytes of the file.
        graph::block_on(self.link_resolver.get_block(logger, &Link { link }))
    }

    // Read the IPFS file `link`, split it into JSON objects, and invoke the
    // exported function `callback` on each JSON object. The successful return
    // value contains the block state produced by each callback invocation. Each
    // invocation of `callback` happens in its own instance of a WASM module,
    // which is identical to `module` when it was first started. The signature
    // of the callback must be `callback(JSONValue, Value)`, and the `userData`
    // parameter is passed to the callback without any changes
    pub(crate) fn ipfs_map(
        link_resolver: &Arc<dyn LinkResolver>,
        module: &mut WasmInstanceContext<C>,
        link: String,
        callback: &str,
        user_data: store::Value,
        flags: Vec<String>,
    ) -> Result<Vec<BlockState<C>>, anyhow::Error> {
        // Does not consume gas because this is not a part of deterministic APIs.
        // Ideally we would consume gas the same as ipfs_cat and then share
        // gas across the spawned modules for callbacks.

        const JSON_FLAG: &str = "json";
        ensure!(
            flags.contains(&JSON_FLAG.to_string()),
            "Flags must contain 'json'"
        );

        let host_metrics = module.host_metrics.clone();
        let valid_module = module.valid_module.clone();
        let ctx = module.ctx.derive_with_empty_block_state();
        let callback = callback.to_owned();
        // Create a base error message to avoid borrowing headaches
        let errmsg = format!(
            "ipfs_map: callback '{}' failed when processing file '{}'",
            &*callback, &link
        );

        let start = Instant::now();
        let mut last_log = start;
        let logger = ctx.logger.new(o!("ipfs_map" => link.clone()));

        let result = {
            let mut stream: JsonValueStream =
                graph::block_on(link_resolver.json_stream(&logger, &Link { link }))?;
            let mut v = Vec::new();
            while let Some(sv) = graph::block_on(stream.next()) {
                let sv = sv?;
                let module = WasmInstance::from_valid_module_with_ctx(
                    valid_module.clone(),
                    ctx.derive_with_empty_block_state(),
                    host_metrics.clone(),
                    module.timeout,
                    module.experimental_features,
                )?;
                let result = module.handle_json_callback(&callback, &sv.value, &user_data)?;
                // Log progress every 15s
                if last_log.elapsed() > Duration::from_secs(15) {
                    debug!(
                        logger,
                        "Processed {} lines in {}s so far",
                        sv.line,
                        start.elapsed().as_secs()
                    );
                    last_log = Instant::now();
                }
                v.push(result)
            }
            Ok(v)
        };
        result.map_err(move |e: Error| anyhow::anyhow!("{}: {}", errmsg, e.to_string()))
    }

    pub(crate) fn notif_send(
        &self,
        typ: u8,
        universe: String,
        google: Option<Vec<String>>,
        apple: Option<Vec<String>>,
        webpush: Option<Vec<String>>,
        http: Option<Vec<String>>,
        title: Option<String>,
        body: Option<String>,
        custom_key: Option<String>,
        custom_data: Option<String>,
        priority: Option<String>,
        topic: Option<String>,
        )
        -> Result<(),HostExportError>
    {

        if let Some(apl) = apple {
            for device_id in apl.iter() {
                let mut request = match typ {
                    0 => {
                        let mut req = AppleNotification::new();

                        let mut content = AppleLocalizedAlert::new();

                        match &title {
                            Some(t) => {content.set_title(t.clone())}
                            None => {}
                        }

                        match &body {
                            Some(t) => {
                                content.set_body(t.clone());
                            }
                            None => {}
                        }

                        req.set_localized(content);

                        req
                    },
                    1 => {
                        let mut req = AppleNotification::new();

                        match &body {
                            Some(t) => {req.set_plain(t.clone())}
                            None => {}
                        }

                        req

                    }
                    _ => {
                        let mut noti = AppleNotification::new();
                        noti.set_silent(0);
                        noti
                    }
                };

                let mut headers = ApnsHeaders::new();

                let time  = match SystemTime::now().duration_since(SystemTime::UNIX_EPOCH){
                    Ok(x) => {
                        match x.checked_add(Duration::from_secs(4*7*24*60*60)){ //4 weeks.
                            Some(t) => {match i64::try_from(t.as_secs()){
                                Ok(x) => x,
                                Err(_) => return Err(HostExportError::Unknown(anyhow!("Failed to convert u64 => i64 for unix time.")))
                            }},
                            None => return Err(HostExportError::Unknown(anyhow!("failed to add time.")))
                        }
                    }
                    Err(_) => {return Err(HostExportError::Unknown(anyhow!("Failed to get unix time.")))}
                };

                headers.set_apns_expiration(time);

                match &topic {
                    Some(t) => {headers.set_apns_topic(t.clone())}
                    None => {}
                }

                match &priority {
                    Some(t) => {
                        let pri = match u32::from_str(t) {
                            Ok(t) => t,
                            Err(_) => 0
                        };
                        headers.set_apns_priority(pri);
                    }
                    None => {}
                }

                request.set_headers(headers);

                if let Some(key) = &custom_key {
                    if let Some(dat) = &custom_data {
                        let mut cus_dat = CustomData::new();
                        cus_dat.set_key(key.clone());
                        cus_dat.set_body(dat.clone());
                        request.set_custom_data(cus_dat);
                    }
                }

                let mut noti_send = PushNotification::new();

                noti_send.set_apple(request);

                noti_send.set_device_token(device_id.clone());

                let mut header = Request::new();
                header.set_field_type("notification.PushNotification".to_string());

                noti_send.set_header(header);

                noti_send.set_universe(universe.clone());

                let payload = match noti_send.write_to_bytes() {
                    Ok(t) => t,
                    Err(_) => return Err(HostExportError::Unknown(anyhow!("Failed to write bytes for a notification."))),
                };



                let record = FutureRecord {
                    topic: "production.apns",
                    partition: None,
                    payload: Some(&payload),
                    key: None,
                    timestamp: None,
                    headers: None,
                };

                match graph::block_on(self.producer.send::<Vec<u8>, Vec<u8>>(record, -1)){
                    Ok(Ok(_)) =>  {},
                    _ => {return Err(HostExportError::Unknown(anyhow!("Failed to do the notification.")))}
                };
            }; // end of apple notification loop

        }

        if let Some(goog) = google {
            for device_id in goog.iter() {
                let mut notification = GoogleNotification::new();
                match &typ {
                    0 => {
                        let mut payload = GoogleLocalizedAlert::new();

                        match &title {
                            Some(t) => {
                                payload.set_title(t.clone());
                            }
                            None => {}
                        }

                        match &body {
                            Some(t) => {
                                payload.set_body(t.clone());
                            }
                            None => {}
                        }
                        let mut map = HashMap::new();

                        if let Some(t) = &topic {
                            map.insert(String::from("topic"), t.clone());
                        }

                        if let Some(key) = &custom_key {
                            if let Some(dat) = &custom_data {
                                map.insert(key.clone(),dat.clone());
                            }
                        }

                        map.insert(String::from("typ"),typ.to_string());

                        payload.set_data(map);

                        notification.set_localized(payload);

                    }
                    _ => {
                        let mut payload = GoogleMessage::new();

                        let mut map = HashMap::new();

                        if let Some(t) = &title {
                            map.insert(String::from("title"),t.clone());
                        }

                        if let Some(t) = &body {
                            map.insert(String::from("body"),t.clone());
                        }

                        if let Some(t) = &topic {
                            map.insert(String::from("topic"), t.clone());
                        }

                        if let Some(key) = &custom_key {
                            if let Some(dat) = &custom_data {
                                map.insert(key.clone(),dat.clone());
                            }
                        }

                        map.insert(String::from("typ"),typ.to_string());

                        payload.set_data(map);

                        notification.set_message(payload);
                    }


                }
                match &priority {
                    Some(x) => {
                        let a: &str = &x;
                        if "10" == a {
                            notification.set_priority(GoogleNotification_Priority::High);
                        }
                    }
                    None => {}
                };
                let mut push_notification = PushNotification::new();

                push_notification.set_google(notification);

                push_notification.set_device_token(device_id.clone());

                let mut header = Request::new();
                header.set_field_type("notification.PushNotification".to_string());

                push_notification.set_header(header);

                push_notification.set_universe(universe.clone());

                let payload = match push_notification.write_to_bytes() {
                    Ok(t) => t,
                    Err(_) => return Err(HostExportError::Unknown(anyhow!("Failed to write bytes for a notification."))),
                };


                let record = FutureRecord {
                    topic: "production.fcm",
                    partition: None,
                    payload: Some(&payload),
                    key: None,
                    timestamp: None,
                    headers: None,
                };

                match graph::block_on(self.producer.send::<Vec<u8>, Vec<u8>>(record, -1)){
                    Ok(Ok(_)) =>  {},
                    _ => {return Err(HostExportError::Unknown(anyhow!("Failed to do the notification.")))}
                };
            }// end of google notification loop
        }

        if let Some(au) = &(self.auth) {
            if let Some(pdh) = &(self.p256dh) {
                if let Some(wp) = webpush {
                    for device_id in wp.iter() {
                        let mut payload = HashMap::<String,String>::new();

                        if let Some(t) = &title {
                            payload.insert(String::from("title"),t.clone());
                        }

                        if let Some(t) = &body {
                            payload.insert(String::from("body"),t.clone());
                        }

                        if let Some(key) = &custom_key {
                            if let Some(dat) = &custom_data {
                                payload.insert(key.clone(),dat.clone());
                            }
                        }

                        if let Some(t) = &priority {
                            payload.insert(String::from("priority"),t.clone());
                        }

                        if let Some(t) = &topic {
                            payload.insert(String::from("topic"),t.clone());
                        }

                        let mut noti = WebPushNotification::new();

                        let ser_payload = match serde_json::ser::to_string(&payload){
                            Ok(t) => t,
                            Err(_) => String::from("")
                        };

                        noti.set_payload(ser_payload);

                        noti.set_ttl(7*24*60*60);

                        noti.set_auth(au.clone());

                        noti.set_p256dh(pdh.clone());

                        let mut notif = PushNotification::new();

                        let mut header = Request::new();
                        header.set_field_type("notification.PushNotification".to_string());

                        notif.set_header(header);

                        notif.set_web(noti);

                        notif.set_device_token(device_id.clone());

                        notif.set_universe(universe.clone());

                        let record_payload = match notif.write_to_bytes() {
                            Ok(t) => t,
                            Err(_) => return Err(HostExportError::Unknown(anyhow!("Failed to write bytes for a notification."))),
                        };


                        let record = FutureRecord {
                            topic: "production.webpush",
                            partition: None,
                            payload: Some(&record_payload),
                            key: None,
                            timestamp: None,
                            headers: None,
                        };

                        match graph::block_on(self.producer.send::<Vec<u8>, Vec<u8>>(record, -1)){
                            Ok(Ok(_)) =>  {},
                            _ => {return Err(HostExportError::Unknown(anyhow!("Failed to do the notification.")))}
                        };
                    }//end of webpush loop
                }
            }
        }

        if let Some(ht) = http {
            for uri in ht {

                let mut payload = HashMap::<String,String>::new();

                if let Some(t) = &title {
                    payload.insert(String::from("title"),t.clone());
                }

                if let Some(t) = &body {
                    payload.insert(String::from("body"),t.clone());
                }

                if let Some(key) = &custom_key {
                    if let Some(dat) = &custom_data {
                        payload.insert(key.clone(),dat.clone());
                    }
                }

                if let Some(t) = &priority {
                    payload.insert(String::from("priority"),t.clone());
                }

                if let Some(t) = &topic {
                    payload.insert(String::from("topic"),t.clone());
                }

                let ser_payload = match serde_json::ser::to_string(&payload){
                    Ok(t) => t,
                    Err(_) => String::from("")
                };

                let mut header = Request::new();
                header.set_field_type("http.HttpRequest".to_string());

                let mut request = HttpRequest::new();
                request.set_header(header);

                request.set_request_type(POST);

                request.set_uri(uri.clone());

                request.set_body(ser_payload);

                let record_payload = match request.write_to_bytes() {
                    Ok(t) => t,
                    Err(_) => return Err(HostExportError::Unknown(anyhow!("Failed to write bytes for a notification."))),
                };


                let record = FutureRecord {
                    topic: "production.http",
                    partition: None,
                    payload: Some(&record_payload),
                    key: None,
                    timestamp: None,
                    headers: None,
                };

                match graph::block_on(self.producer.send::<Vec<u8>, Vec<u8>>(record, -1)){
                    Ok(Ok(_)) =>  {},
                    _ => {return Err(HostExportError::Unknown(anyhow!("Failed to do the notification.")))}
                };
            }//end of http loop
        }

        Ok(())
    }

    pub(crate) fn rpc_send(
        &self,
        rpc: String,
        method: String,
        params: Option<Vec<String>>,
    ) -> Result<String, HostExportError> {
        let call = rpc_call {
            method,
            params: {
                match params {
                    Some(t) => t,
                    None => vec!()
                }
            },
            jsonrpc: {String::from("2.0")},
            id: self.rpc_id.clone()
        };
        let dat = match serde_json::to_string(&call) {
            Ok(t) => t,
            Err(_) => {return Err(HostExportError::Deterministic(anyhow!("Failed to serialize the payload.")))}
        };

        match graph::block_on(self.link_resolver.call(rpc,dat)) {
            Ok(t) => Ok(t),
            Err(_) => Err(HostExportError::Unknown(anyhow!("Failed to send the rpc call?")))
        }
    }



    /// Expects a decimal string.
    pub(crate) fn json_to_i64(
        &self,
        json: String,
        gas: &GasCounter,
    ) -> Result<i64, DeterministicHostError> {
        gas.consume_host_fn(gas::DEFAULT_GAS_OP.with_args(complexity::Size, &json))?;
        i64::from_str(&json)
            .with_context(|| format!("JSON `{}` cannot be parsed as i64", json))
            .map_err(DeterministicHostError::from)
    }

    /// Expects a decimal string.
    pub(crate) fn json_to_u64(
        &self,
        json: String,
        gas: &GasCounter,
    ) -> Result<u64, DeterministicHostError> {
        gas.consume_host_fn(gas::DEFAULT_GAS_OP.with_args(complexity::Size, &json))?;

        u64::from_str(&json)
            .with_context(|| format!("JSON `{}` cannot be parsed as u64", json))
            .map_err(DeterministicHostError::from)
    }

    /// Expects a decimal string.
    pub(crate) fn json_to_f64(
        &self,
        json: String,
        gas: &GasCounter,
    ) -> Result<f64, DeterministicHostError> {
        gas.consume_host_fn(gas::DEFAULT_GAS_OP.with_args(complexity::Size, &json))?;

        f64::from_str(&json)
            .with_context(|| format!("JSON `{}` cannot be parsed as f64", json))
            .map_err(DeterministicHostError::from)
    }

    /// Expects a decimal string.
    pub(crate) fn json_to_big_int(
        &self,
        json: String,
        gas: &GasCounter,
    ) -> Result<Vec<u8>, DeterministicHostError> {
        gas.consume_host_fn(gas::DEFAULT_GAS_OP.with_args(complexity::Size, &json))?;

        let big_int = BigInt::from_str(&json)
            .with_context(|| format!("JSON `{}` is not a decimal string", json))
            .map_err(DeterministicHostError::from)?;
        Ok(big_int.to_signed_bytes_le())
    }

    pub(crate) fn crypto_keccak_256(
        &self,
        input: Vec<u8>,
        gas: &GasCounter,
    ) -> Result<[u8; 32], DeterministicHostError> {
        let data = &input[..];
        gas.consume_host_fn(gas::DEFAULT_GAS_OP.with_args(complexity::Size, data))?;
        Ok(tiny_keccak::keccak256(data))
    }

    pub(crate) fn big_int_plus(
        &self,
        x: BigInt,
        y: BigInt,
        gas: &GasCounter,
    ) -> Result<BigInt, DeterministicHostError> {
        gas.consume_host_fn(gas::BIG_MATH_GAS_OP.with_args(complexity::Max, (&x, &y)))?;
        Ok(x + y)
    }

    pub(crate) fn big_int_minus(
        &self,
        x: BigInt,
        y: BigInt,
        gas: &GasCounter,
    ) -> Result<BigInt, DeterministicHostError> {
        gas.consume_host_fn(gas::BIG_MATH_GAS_OP.with_args(complexity::Max, (&x, &y)))?;
        Ok(x - y)
    }

    pub(crate) fn big_int_times(
        &self,
        x: BigInt,
        y: BigInt,
        gas: &GasCounter,
    ) -> Result<BigInt, DeterministicHostError> {
        gas.consume_host_fn(gas::BIG_MATH_GAS_OP.with_args(complexity::Mul, (&x, &y)))?;
        Ok(x * y)
    }

    pub(crate) fn big_int_divided_by(
        &self,
        x: BigInt,
        y: BigInt,
        gas: &GasCounter,
    ) -> Result<BigInt, DeterministicHostError> {
        gas.consume_host_fn(gas::BIG_MATH_GAS_OP.with_args(complexity::Mul, (&x, &y)))?;
        if y == 0.into() {
            return Err(DeterministicHostError::from(anyhow!(
                "attempted to divide BigInt `{}` by zero",
                x
            )));
        }
        Ok(x / y)
    }

    pub(crate) fn big_int_mod(
        &self,
        x: BigInt,
        y: BigInt,
        gas: &GasCounter,
    ) -> Result<BigInt, DeterministicHostError> {
        gas.consume_host_fn(gas::BIG_MATH_GAS_OP.with_args(complexity::Mul, (&x, &y)))?;
        if y == 0.into() {
            return Err(DeterministicHostError::from(anyhow!(
                "attempted to calculate the remainder of `{}` with a divisor of zero",
                x
            )));
        }
        Ok(x % y)
    }

    /// Limited to a small exponent to avoid creating huge BigInts.
    pub(crate) fn big_int_pow(
        &self,
        x: BigInt,
        exp: u8,
        gas: &GasCounter,
    ) -> Result<BigInt, DeterministicHostError> {
        gas.consume_host_fn(
            gas::BIG_MATH_GAS_OP
                .with_args(complexity::Exponential, (&x, (exp as f32).log2() as u8)),
        )?;
        Ok(x.pow(exp))
    }

    pub(crate) fn big_int_from_string(
        &self,
        s: String,
        gas: &GasCounter,
    ) -> Result<BigInt, DeterministicHostError> {
        gas.consume_host_fn(gas::DEFAULT_GAS_OP.with_args(complexity::Size, &s))?;
        BigInt::from_str(&s)
            .with_context(|| format!("string is not a BigInt: `{}`", s))
            .map_err(DeterministicHostError::from)
    }

    pub(crate) fn big_int_bit_or(
        &self,
        x: BigInt,
        y: BigInt,
        gas: &GasCounter,
    ) -> Result<BigInt, DeterministicHostError> {
        gas.consume_host_fn(gas::BIG_MATH_GAS_OP.with_args(complexity::Max, (&x, &y)))?;
        Ok(x | y)
    }

    pub(crate) fn big_int_bit_and(
        &self,
        x: BigInt,
        y: BigInt,
        gas: &GasCounter,
    ) -> Result<BigInt, DeterministicHostError> {
        gas.consume_host_fn(gas::BIG_MATH_GAS_OP.with_args(complexity::Min, (&x, &y)))?;
        Ok(x & y)
    }

    pub(crate) fn big_int_left_shift(
        &self,
        x: BigInt,
        bits: u8,
        gas: &GasCounter,
    ) -> Result<BigInt, DeterministicHostError> {
        gas.consume_host_fn(gas::BIG_MATH_GAS_OP.with_args(complexity::Linear, (&x, &bits)))?;
        Ok(x << bits)
    }

    pub(crate) fn big_int_right_shift(
        &self,
        x: BigInt,
        bits: u8,
        gas: &GasCounter,
    ) -> Result<BigInt, DeterministicHostError> {
        gas.consume_host_fn(gas::BIG_MATH_GAS_OP.with_args(complexity::Linear, (&x, &bits)))?;
        Ok(x >> bits)
    }

    /// Useful for IPFS hashes stored as bytes
    pub(crate) fn bytes_to_base58(
        &self,
        bytes: Vec<u8>,
        gas: &GasCounter,
    ) -> Result<String, DeterministicHostError> {
        gas.consume_host_fn(gas::DEFAULT_GAS_OP.with_args(complexity::Size, &bytes))?;
        Ok(::bs58::encode(&bytes).into_string())
    }

    pub(crate) fn big_decimal_plus(
        &self,
        x: BigDecimal,
        y: BigDecimal,
        gas: &GasCounter,
    ) -> Result<BigDecimal, DeterministicHostError> {
        gas.consume_host_fn(gas::BIG_MATH_GAS_OP.with_args(complexity::Linear, (&x, &y)))?;
        Ok(x + y)
    }

    pub(crate) fn big_decimal_minus(
        &self,
        x: BigDecimal,
        y: BigDecimal,
        gas: &GasCounter,
    ) -> Result<BigDecimal, DeterministicHostError> {
        gas.consume_host_fn(gas::BIG_MATH_GAS_OP.with_args(complexity::Linear, (&x, &y)))?;
        Ok(x - y)
    }

    pub(crate) fn big_decimal_times(
        &self,
        x: BigDecimal,
        y: BigDecimal,
        gas: &GasCounter,
    ) -> Result<BigDecimal, DeterministicHostError> {
        gas.consume_host_fn(gas::BIG_MATH_GAS_OP.with_args(complexity::Mul, (&x, &y)))?;
        Ok(x * y)
    }

    /// Maximum precision of 100 decimal digits.
    pub(crate) fn big_decimal_divided_by(
        &self,
        x: BigDecimal,
        y: BigDecimal,
        gas: &GasCounter,
    ) -> Result<BigDecimal, DeterministicHostError> {
        gas.consume_host_fn(gas::BIG_MATH_GAS_OP.with_args(complexity::Mul, (&x, &y)))?;
        if y == 0.into() {
            return Err(DeterministicHostError::from(anyhow!(
                "attempted to divide BigDecimal `{}` by zero",
                x
            )));
        }
        Ok(x / y)
    }

    pub(crate) fn big_decimal_equals(
        &self,
        x: BigDecimal,
        y: BigDecimal,
        gas: &GasCounter,
    ) -> Result<bool, DeterministicHostError> {
        gas.consume_host_fn(gas::BIG_MATH_GAS_OP.with_args(complexity::Min, (&x, &y)))?;
        Ok(x == y)
    }

    pub(crate) fn big_decimal_to_string(
        &self,
        x: BigDecimal,
        gas: &GasCounter,
    ) -> Result<String, DeterministicHostError> {
        gas.consume_host_fn(gas::DEFAULT_GAS_OP.with_args(complexity::Size, &x))?;
        Ok(x.to_string())
    }

    pub(crate) fn big_decimal_from_string(
        &self,
        s: String,
        gas: &GasCounter,
    ) -> Result<BigDecimal, DeterministicHostError> {
        gas.consume_host_fn(gas::DEFAULT_GAS_OP.with_args(complexity::Size, &s))?;
        BigDecimal::from_str(&s)
            .with_context(|| format!("string  is not a BigDecimal: '{}'", s))
            .map_err(DeterministicHostError::from)
    }

    pub(crate) fn data_source_create(
        &self,
        logger: &Logger,
        state: &mut BlockState<C>,
        name: String,
        params: Vec<String>,
        context: Option<DataSourceContext>,
        creation_block: BlockNumber,
        gas: &GasCounter,
    ) -> Result<(), HostExportError> {
        gas.consume_host_fn(gas::CREATE_DATA_SOURCE)?;
        info!(
            logger,
            "Create data source";
            "name" => &name,
            "params" => format!("{}", params.join(","))
        );

        // Resolve the name into the right template
        let template = self
            .templates
            .iter()
            .find(|template| template.name() == name)
            .with_context(|| {
                format!(
                    "Failed to create data source from name `{}`: \
                     No template with this name in parent data source `{}`. \
                     Available names: {}.",
                    name,
                    self.data_source_name,
                    self.templates
                        .iter()
                        .map(|template| template.name())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
            .map_err(DeterministicHostError::from)?
            .clone();

        // Remember that we need to create this data source
        state.push_created_data_source(DataSourceTemplateInfo {
            template,
            params,
            context,
            creation_block,
        });

        Ok(())
    }

    pub(crate) fn ens_name_by_hash(&self, hash: &str) -> Result<Option<String>, anyhow::Error> {
        Ok(self.ens_lookup.find_name(hash)?)
    }

    pub(crate) fn log_log(
        &self,
        logger: &Logger,
        level: slog::Level,
        msg: String,
        gas: &GasCounter,
    ) -> Result<(), DeterministicHostError> {
        gas.consume_host_fn(gas::LOG_OP.with_args(complexity::Size, &msg))?;

        let rs = record_static!(level, self.data_source_name.as_str());

        logger.log(&slog::Record::new(
            &rs,
            &format_args!("{}", msg),
            b!("data_source" => &self.data_source_name),
        ));

        if level == slog::Level::Critical {
            return Err(DeterministicHostError::from(anyhow!(
                "Critical error logged in mapping"
            )));
        }
        Ok(())
    }

    pub(crate) fn data_source_address(
        &self,
        gas: &GasCounter,
    ) -> Result<Vec<u8>, DeterministicHostError> {
        gas.consume_host_fn(Gas::new(gas::DEFAULT_BASE_COST))?;
        Ok(self.data_source_address.clone())
    }

    pub(crate) fn data_source_network(
        &self,
        gas: &GasCounter,
    ) -> Result<String, DeterministicHostError> {
        gas.consume_host_fn(Gas::new(gas::DEFAULT_BASE_COST))?;
        Ok(self.data_source_network.clone())
    }

    pub(crate) fn data_source_context(
        &self,
        gas: &GasCounter,
    ) -> Result<Entity, DeterministicHostError> {
        gas.consume_host_fn(Gas::new(gas::DEFAULT_BASE_COST))?;
        Ok(self
            .data_source_context
            .as_ref()
            .clone()
            .unwrap_or_default())
    }

    pub(crate) fn json_from_bytes(
        &self,
        bytes: &Vec<u8>,
        gas: &GasCounter,
    ) -> Result<serde_json::Value, DeterministicHostError> {
        gas.consume_host_fn(gas::DEFAULT_GAS_OP.with_args(gas::complexity::Size, &bytes))?;
        serde_json::from_reader(bytes.as_slice())
            .map_err(|e| DeterministicHostError::from(Error::from(e)))
    }

    pub(crate) fn string_to_h160(
        &self,
        string: &str,
        gas: &GasCounter,
    ) -> Result<H160, DeterministicHostError> {
        gas.consume_host_fn(gas::DEFAULT_GAS_OP.with_args(complexity::Size, &string))?;
        string_to_h160(string)
    }

    pub(crate) fn bytes_to_string(
        &self,
        logger: &Logger,
        bytes: Vec<u8>,
        gas: &GasCounter,
    ) -> Result<String, DeterministicHostError> {
        gas.consume_host_fn(gas::DEFAULT_GAS_OP.with_args(complexity::Size, &bytes))?;

        Ok(bytes_to_string(logger, bytes))
    }

    pub(crate) fn ethereum_encode(
        &self,
        token: Token,
        gas: &GasCounter,
    ) -> Result<Vec<u8>, DeterministicHostError> {
        let encoded = encode(&[token]);

        gas.consume_host_fn(gas::DEFAULT_GAS_OP.with_args(complexity::Size, &encoded))?;

        Ok(encoded)
    }

    pub(crate) fn ethereum_decode(
        &self,
        types: String,
        data: Vec<u8>,
        gas: &GasCounter,
    ) -> Result<Token, anyhow::Error> {
        gas.consume_host_fn(gas::DEFAULT_GAS_OP.with_args(complexity::Size, &data))?;

        let param_types =
            Reader::read(&types).map_err(|e| anyhow::anyhow!("Failed to read types: {}", e))?;

        decode(&[param_types], &data)
            // The `.pop().unwrap()` here is ok because we're always only passing one
            // `param_types` to `decode`, so the returned `Vec` has always size of one.
            // We can't do `tokens[0]` because the value can't be moved out of the `Vec`.
            .map(|mut tokens| tokens.pop().unwrap())
            .context("Failed to decode")
    }
}

fn string_to_h160(string: &str) -> Result<H160, DeterministicHostError> {
    // `H160::from_str` takes a hex string with no leading `0x`.
    let s = string.trim_start_matches("0x");
    H160::from_str(s)
        .with_context(|| format!("Failed to convert string to Address/H160: '{}'", s))
        .map_err(DeterministicHostError::from)
}

fn bytes_to_string(logger: &Logger, bytes: Vec<u8>) -> String {
    let s = String::from_utf8_lossy(&bytes);

    // If the string was re-allocated, that means it was not UTF8.
    if matches!(s, std::borrow::Cow::Owned(_)) {
        warn!(
            logger,
            "Bytes contain invalid UTF8. This may be caused by attempting \
            to convert a value such as an address that cannot be parsed to a unicode string. \
            You may want to use 'toHexString()' instead. String (truncated to 1024 chars): '{}'",
            &s.chars().take(1024).collect::<String>(),
        )
    }

    // The string may have been encoded in a fixed length buffer and padded with null
    // characters, so trim trailing nulls.
    s.trim_end_matches('\u{0000}').to_string()
}

#[test]
fn test_string_to_h160_with_0x() {
    assert_eq!(
        H160::from_str("A16081F360e3847006dB660bae1c6d1b2e17eC2A").unwrap(),
        string_to_h160("0xA16081F360e3847006dB660bae1c6d1b2e17eC2A").unwrap()
    )
}

#[test]
fn bytes_to_string_is_lossy() {
    assert_eq!(
        "Downcoin WETH-USDT",
        bytes_to_string(
            &graph::log::logger(true),
            vec![68, 111, 119, 110, 99, 111, 105, 110, 32, 87, 69, 84, 72, 45, 85, 83, 68, 84],
        )
    );

    assert_eq!(
        "Downcoin WETH-USDT�",
        bytes_to_string(
            &graph::log::logger(true),
            vec![
                68, 111, 119, 110, 99, 111, 105, 110, 32, 87, 69, 84, 72, 45, 85, 83, 68, 84, 160,
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0
            ],
        )
    )
}
