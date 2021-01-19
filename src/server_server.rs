use crate::{client_server, utils, ConduitResult, Database, Error, PduEvent, Result, Ruma};
use get_profile_information::v1::ProfileField;
use http::header::{HeaderValue, AUTHORIZATION, HOST};
use log::{error, info, warn};
use rocket::{get, post, put, response::content::Json, State};
use ruma::{
    api::{
        federation::{
            directory::{get_public_rooms, get_public_rooms_filtered},
            discovery::{
                get_server_keys, get_server_version::v1 as get_server_version, ServerSigningKeys,
                VerifyKey,
            },
            event::{get_event, get_missing_events, get_room_state_ids},
            query::get_profile_information,
            transactions::send_transaction_message,
        },
        OutgoingRequest,
    },
    directory::{IncomingFilter, IncomingRoomNetwork},
    serde::to_canonical_value,
    signatures::{CanonicalJsonObject, CanonicalJsonValue, PublicKeyMap},
    EventId, RoomId, RoomVersionId, ServerName, ServerSigningKeyId, UserId,
};
use state_res::{Event, EventMap, StateMap};
use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    convert::TryFrom,
    fmt::Debug,
    future::Future,
    net::{IpAddr, SocketAddr},
    pin::Pin,
    result::Result as StdResult,
    sync::Arc,
    time::{Duration, SystemTime},
};

pub async fn send_request<T: OutgoingRequest>(
    globals: &crate::database::globals::Globals,
    destination: &ServerName,
    request: T,
) -> Result<T::IncomingResponse>
where
    T: Debug,
{
    if !globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    let maybe_result = globals
        .actual_destination_cache
        .read()
        .unwrap()
        .get(destination)
        .cloned();

    let (actual_destination, host) = if let Some(result) = maybe_result {
        result
    } else {
        let result = find_actual_destination(globals, &destination).await;
        globals
            .actual_destination_cache
            .write()
            .unwrap()
            .insert(Box::<ServerName>::from(destination), result.clone());
        result
    };

    let mut http_request = request
        .try_into_http_request(&actual_destination, Some(""))
        .map_err(|e| {
            warn!("Failed to find destination {}: {}", actual_destination, e);
            Error::BadServerResponse("Invalid destination")
        })?;

    let mut request_map = serde_json::Map::new();

    if !http_request.body().is_empty() {
        request_map.insert(
            "content".to_owned(),
            serde_json::from_slice(http_request.body())
                .expect("body is valid json, we just created it"),
        );
    };

    request_map.insert("method".to_owned(), T::METADATA.method.to_string().into());
    request_map.insert(
        "uri".to_owned(),
        http_request
            .uri()
            .path_and_query()
            .expect("all requests have a path")
            .to_string()
            .into(),
    );
    request_map.insert("origin".to_owned(), globals.server_name().as_str().into());
    request_map.insert("destination".to_owned(), destination.as_str().into());

    let mut request_json =
        serde_json::from_value(request_map.into()).expect("valid JSON is valid BTreeMap");

    ruma::signatures::sign_json(
        globals.server_name().as_str(),
        globals.keypair(),
        &mut request_json,
    )
    .expect("our request json is what ruma expects");

    let request_json: serde_json::Map<String, serde_json::Value> =
        serde_json::from_slice(&serde_json::to_vec(&request_json).unwrap()).unwrap();

    let signatures = request_json["signatures"]
        .as_object()
        .unwrap()
        .values()
        .map(|v| {
            v.as_object()
                .unwrap()
                .iter()
                .map(|(k, v)| (k, v.as_str().unwrap()))
        });

    for signature_server in signatures {
        for s in signature_server {
            http_request.headers_mut().insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!(
                    "X-Matrix origin={},key=\"{}\",sig=\"{}\"",
                    globals.server_name(),
                    s.0,
                    s.1
                ))
                .unwrap(),
            );
        }
    }

    if let Some(host) = host {
        http_request
            .headers_mut()
            .insert(HOST, HeaderValue::from_str(&host).unwrap());
    }

    let mut reqwest_request = reqwest::Request::try_from(http_request)
        .expect("all http requests are valid reqwest requests");

    *reqwest_request.timeout_mut() = Some(Duration::from_secs(30));

    let url = reqwest_request.url().clone();
    let reqwest_response = globals.reqwest_client().execute(reqwest_request).await;

    // Because reqwest::Response -> http::Response is complicated:
    match reqwest_response {
        Ok(mut reqwest_response) => {
            let status = reqwest_response.status();
            let mut http_response = http::Response::builder().status(status);
            let headers = http_response.headers_mut().unwrap();

            for (k, v) in reqwest_response.headers_mut().drain() {
                if let Some(key) = k {
                    headers.insert(key, v);
                }
            }

            let status = reqwest_response.status();

            let body = reqwest_response
                .bytes()
                .await
                .unwrap_or_else(|e| {
                    warn!("server error {}", e);
                    Vec::new().into()
                }) // TODO: handle timeout
                .into_iter()
                .collect::<Vec<_>>();

            if status != 200 {
                info!(
                    "Server returned bad response {} {}\n{}\n{:?}",
                    destination,
                    status,
                    url,
                    utils::string_from_bytes(&body)
                );
            }

            let response = T::IncomingResponse::try_from(
                http_response
                    .body(body)
                    .expect("reqwest body is valid http body"),
            );
            response.map_err(|_| {
                info!(
                    "Server returned invalid response bytes {}\n{}",
                    destination, url
                );
                Error::BadServerResponse("Server returned bad response.")
            })
        }
        Err(e) => Err(e.into()),
    }
}

fn get_ip_with_port(destination_str: String) -> Option<String> {
    if destination_str.parse::<SocketAddr>().is_ok() {
        Some(destination_str)
    } else if let Ok(ip_addr) = destination_str.parse::<IpAddr>() {
        Some(SocketAddr::new(ip_addr, 8448).to_string())
    } else {
        None
    }
}

fn add_port_to_hostname(destination_str: String) -> String {
    match destination_str.find(':') {
        None => destination_str.to_owned() + ":8448",
        Some(_) => destination_str.to_string(),
    }
}

/// Returns: actual_destination, host header
/// Implemented according to the specification at https://matrix.org/docs/spec/server_server/r0.1.4#resolving-server-names
/// Numbers in comments below refer to bullet points in linked section of specification
async fn find_actual_destination(
    globals: &crate::database::globals::Globals,
    destination: &ServerName,
) -> (String, Option<String>) {
    let mut host = None;

    let destination_str = destination.as_str().to_owned();
    let actual_destination = "https://".to_owned()
        + &match get_ip_with_port(destination_str.clone()) {
            Some(host_port) => {
                // 1: IP literal with provided or default port
                host_port
            }
            None => {
                if destination_str.find(':').is_some() {
                    // 2: Hostname with included port
                    destination_str
                } else {
                    match request_well_known(globals, &destination.as_str()).await {
                        // 3: A .well-known file is available
                        Some(delegated_hostname) => {
                            match get_ip_with_port(delegated_hostname.clone()) {
                                Some(host_and_port) => host_and_port, // 3.1: IP literal in .well-known file
                                None => {
                                    if destination_str.find(':').is_some() {
                                        // 3.2: Hostname with port in .well-known file
                                        destination_str
                                    } else {
                                        match query_srv_record(globals, &delegated_hostname).await {
                                            // 3.3: SRV lookup successful
                                            Some(hostname) => hostname,
                                            // 3.4: No SRV records, just use the hostname from .well-known
                                            None => add_port_to_hostname(delegated_hostname),
                                        }
                                    }
                                }
                            }
                        }
                        // 4: No .well-known or an error occured
                        None => {
                            match query_srv_record(globals, &destination_str).await {
                                // 4: SRV record found
                                Some(hostname) => {
                                    host = Some(destination_str.to_owned());
                                    hostname
                                }
                                // 5: No SRV record found
                                None => add_port_to_hostname(destination_str.to_string()),
                            }
                        }
                    }
                }
            }
        };

    (actual_destination, host)
}

async fn query_srv_record(
    globals: &crate::database::globals::Globals,
    hostname: &str,
) -> Option<String> {
    if let Ok(Some(host_port)) = globals
        .dns_resolver()
        .srv_lookup(format!("_matrix._tcp.{}", hostname))
        .await
        .map(|srv| {
            srv.iter().next().map(|result| {
                format!(
                    "{}:{}",
                    result.target().to_string().trim_end_matches('.'),
                    result.port().to_string()
                )
            })
        })
    {
        Some(host_port)
    } else {
        None
    }
}

pub async fn request_well_known(
    globals: &crate::database::globals::Globals,
    destination: &str,
) -> Option<String> {
    let body: serde_json::Value = serde_json::from_str(
        &globals
            .reqwest_client()
            .get(&format!(
                "https://{}/.well-known/matrix/server",
                destination
            ))
            .send()
            .await
            .ok()?
            .text()
            .await
            .ok()?,
    )
    .ok()?;
    Some(body.get("m.server")?.as_str()?.to_owned())
}

#[cfg_attr(feature = "conduit_bin", get("/_matrix/federation/v1/version"))]
pub fn get_server_version_route(
    db: State<'_, Database>,
) -> ConduitResult<get_server_version::Response> {
    if !db.globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    Ok(get_server_version::Response {
        server: Some(get_server_version::Server {
            name: Some("Conduit".to_owned()),
            version: Some(env!("CARGO_PKG_VERSION").to_owned()),
        }),
    }
    .into())
}

#[cfg_attr(feature = "conduit_bin", get("/_matrix/key/v2/server"))]
pub fn get_server_keys_route(db: State<'_, Database>) -> Json<String> {
    if !db.globals.allow_federation() {
        // TODO: Use proper types
        return Json("Federation is disabled.".to_owned());
    }

    let mut verify_keys = BTreeMap::new();
    verify_keys.insert(
        ServerSigningKeyId::try_from(
            format!("ed25519:{}", db.globals.keypair().version()).as_str(),
        )
        .expect("found invalid server signing keys in DB"),
        VerifyKey {
            key: base64::encode_config(db.globals.keypair().public_key(), base64::STANDARD_NO_PAD),
        },
    );
    let mut response = serde_json::from_slice(
        http::Response::try_from(get_server_keys::v2::Response {
            server_key: ServerSigningKeys {
                server_name: db.globals.server_name().to_owned(),
                verify_keys,
                old_verify_keys: BTreeMap::new(),
                signatures: BTreeMap::new(),
                valid_until_ts: SystemTime::now() + Duration::from_secs(60 * 2),
            },
        })
        .unwrap()
        .body(),
    )
    .unwrap();

    ruma::signatures::sign_json(
        db.globals.server_name().as_str(),
        db.globals.keypair(),
        &mut response,
    )
    .unwrap();

    Json(ruma::serde::to_canonical_json_string(&response).expect("JSON is canonical"))
}

#[cfg_attr(feature = "conduit_bin", get("/_matrix/key/v2/server/<_>"))]
pub fn get_server_keys_deprecated_route(db: State<'_, Database>) -> Json<String> {
    get_server_keys_route(db)
}

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/federation/v1/publicRooms", data = "<body>")
)]
pub async fn get_public_rooms_filtered_route(
    db: State<'_, Database>,
    body: Ruma<get_public_rooms_filtered::v1::Request<'_>>,
) -> ConduitResult<get_public_rooms_filtered::v1::Response> {
    if !db.globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    let response = client_server::get_public_rooms_filtered_helper(
        &db,
        None,
        body.limit,
        body.since.as_deref(),
        &body.filter,
        &body.room_network,
    )
    .await?
    .0;

    Ok(get_public_rooms_filtered::v1::Response {
        chunk: response
            .chunk
            .into_iter()
            .map(|c| {
                // Convert ruma::api::federation::directory::get_public_rooms::v1::PublicRoomsChunk
                // to ruma::api::client::r0::directory::PublicRoomsChunk
                Ok::<_, Error>(
                    serde_json::from_str(
                        &serde_json::to_string(&c)
                            .expect("PublicRoomsChunk::to_string always works"),
                    )
                    .expect("federation and client-server PublicRoomsChunk are the same type"),
                )
            })
            .filter_map(|r| r.ok())
            .collect(),
        prev_batch: response.prev_batch,
        next_batch: response.next_batch,
        total_room_count_estimate: response.total_room_count_estimate,
    }
    .into())
}

#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/federation/v1/publicRooms", data = "<body>")
)]
pub async fn get_public_rooms_route(
    db: State<'_, Database>,
    body: Ruma<get_public_rooms::v1::Request<'_>>,
) -> ConduitResult<get_public_rooms::v1::Response> {
    if !db.globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    let response = client_server::get_public_rooms_filtered_helper(
        &db,
        None,
        body.limit,
        body.since.as_deref(),
        &IncomingFilter::default(),
        &IncomingRoomNetwork::Matrix,
    )
    .await?
    .0;

    Ok(get_public_rooms::v1::Response {
        chunk: response
            .chunk
            .into_iter()
            .map(|c| {
                // Convert ruma::api::federation::directory::get_public_rooms::v1::PublicRoomsChunk
                // to ruma::api::client::r0::directory::PublicRoomsChunk
                Ok::<_, Error>(
                    serde_json::from_str(
                        &serde_json::to_string(&c)
                            .expect("PublicRoomsChunk::to_string always works"),
                    )
                    .expect("federation and client-server PublicRoomsChunk are the same type"),
                )
            })
            .filter_map(|r| r.ok())
            .collect(),
        prev_batch: response.prev_batch,
        next_batch: response.next_batch,
        total_room_count_estimate: response.total_room_count_estimate,
    }
    .into())
}

#[derive(Clone, Debug, Ord, PartialOrd, Eq, PartialEq)]
pub enum PrevEvents<T> {
    Sequential(T),
    Fork(Vec<T>),
}

impl<T> IntoIterator for PrevEvents<T> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<Self::Item>;

    fn into_iter(self) -> Self::IntoIter {
        match self {
            Self::Sequential(item) => vec![item].into_iter(),
            Self::Fork(list) => list.into_iter(),
        }
    }
}

impl<T: Clone> PrevEvents<T> {
    pub fn new(id: &[T]) -> Self {
        match id {
            [] => panic!("All events must have previous event"),
            [single_id] => Self::Sequential(single_id.clone()),
            rest => Self::Fork(rest.to_vec()),
        }
    }
}

#[cfg_attr(
    feature = "conduit_bin",
    put("/_matrix/federation/v1/send/<_>", data = "<body>")
)]
pub async fn send_transaction_message_route<'a>(
    db: State<'a, Database>,
    body: Ruma<send_transaction_message::v1::Request<'_>>,
) -> ConduitResult<send_transaction_message::v1::Response> {
    if !db.globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    dbg!(&*body);

    for edu in &body.edus {
        match serde_json::from_str::<send_transaction_message::v1::Edu>(edu.json().get()) {
            Ok(edu) => match edu.edu_type.as_str() {
                "m.typing" => {
                    if let Some(typing) = edu.content.get("typing") {
                        if typing.as_bool().unwrap_or_default() {
                            db.rooms.edus.typing_add(
                                &UserId::try_from(edu.content["user_id"].as_str().unwrap())
                                    .unwrap(),
                                &RoomId::try_from(edu.content["room_id"].as_str().unwrap())
                                    .unwrap(),
                                3000 + utils::millis_since_unix_epoch(),
                                &db.globals,
                            )?;
                        } else {
                            db.rooms.edus.typing_remove(
                                &UserId::try_from(edu.content["user_id"].as_str().unwrap())
                                    .unwrap(),
                                &RoomId::try_from(edu.content["room_id"].as_str().unwrap())
                                    .unwrap(),
                                &db.globals,
                            )?;
                        }
                    }
                }
                "m.presence" => {}
                "m.receipt" => {}
                "m.device_list_update" => {}
                _ => {}
            },
            Err(_err) => {
                continue;
            }
        }
    }

    // TODO: For RoomVersion6 we must check that Raw<..> is canonical do we anywhere?
    // SPEC:
    // Servers MUST strictly enforce the JSON format specified in the appendices.
    // This translates to a 400 M_BAD_JSON error on most endpoints, or discarding of
    // events over federation. For example, the Federation API's /send endpoint would
    // discard the event whereas the Client Server API's /send/{eventType} endpoint
    // would return a M_BAD_JSON error.
    let mut resolved_map = BTreeMap::new();
    'main_pdu_loop: for pdu in &body.pdus {
        // 1. Is a valid event, otherwise it is dropped.
        // Ruma/PduEvent/StateEvent satisfies this
        // We do not add the event_id field to the pdu here because of signature and hashes checks
        let (event_id, value) = crate::pdu::gen_event_id_canonical_json(pdu);

        // If we have no idea about this room skip the PDU
        let room_id = match value
            .get("room_id")
            .map(|id| match id {
                CanonicalJsonValue::String(id) => RoomId::try_from(id.as_str()).ok(),
                _ => None,
            })
            .flatten()
        {
            Some(id) => id,
            None => {
                resolved_map.insert(event_id, Err("Event needs a valid RoomId".to_string()));
                continue;
            }
        };

        // 1. check the server is in the room (optional)
        if !db.rooms.exists(&room_id)? {
            resolved_map.insert(event_id, Err("Room is unknown to this server".to_string()));
            continue;
        }

        let server_name = &body.body.origin;
        let mut pub_key_map = BTreeMap::new();

        if let Some(CanonicalJsonValue::String(sender)) = value.get("sender") {
            let sender =
                UserId::try_from(sender.as_str()).expect("All PDUs have a valid sender field");
            let origin = sender.server_name();

            // TODO: this could fail or the server not respond...
            let keys = fetch_signing_keys(&db, origin).await?;

            pub_key_map.insert(
                origin.to_string(),
                keys.into_iter()
                    .map(|(k, v)| (k.to_string(), v.key))
                    .collect(),
            );
        } else {
            resolved_map.insert(event_id, Err("No field `signatures` in JSON".to_string()));
            continue;
        }

        // TODO: make this persist but not a DB Tree...
        // This is all the auth_events that have been recursively fetched so they don't have to be
        // deserialized over and over again. This could potentially also be some sort of trie (suffix tree)
        // like structure so that once an auth event is known it would know (using indexes maybe) all of
        // the auth events that it references.
        let mut auth_cache = EventMap::new();

        // 2. check content hash, redact if doesn't match
        // 3. fetch any missing auth events doing all checks listed here starting at 1. These are not timeline events
        // 4. reject "due to auth events" if can't get all the auth events or some of the auth events are also rejected "due to auth events"
        // 5. reject "due to auth events" if the event doesn't pass auth based on the auth events
        // 7. if not timeline event: stop
        // TODO; 8. fetch any missing prev events doing all checks listed here starting at 1. These are timeline events
        // the events found in step 8 can be authed/resolved and appended to the DB
        let (pdu, previous): (_, Vec<Arc<PduEvent>>) = match validate_event(
            &db,
            value,
            event_id.clone(),
            &pub_key_map,
            server_name,
            // All the auth events gathered will be here
            &mut auth_cache,
        )
        .await
        {
            Ok(pdu) => pdu,
            Err(e) => {
                resolved_map.insert(event_id, Err(e));
                continue;
            }
        };

        let single_prev = if previous.len() == 1 {
            previous.first().cloned()
        } else {
            None
        };

        // 6. persist the event as an outlier.
        db.rooms.append_pdu_outlier(pdu.event_id(), &pdu)?;

        // Step 9. fetch missing state by calling /state_ids at backwards extremities doing all
        // the checks in this list starting at 1. These are not timeline events.
        //
        // Step 10. check the auth of the event passes based on the calculated state of the event
        let (state_at_event, incoming_auth_events): (StateMap<Arc<PduEvent>>, Vec<Arc<PduEvent>>) =
            match db
                .sending
                .send_federation_request(
                    &db.globals,
                    server_name,
                    get_room_state_ids::v1::Request {
                        room_id: pdu.room_id(),
                        event_id: pdu.event_id(),
                    },
                )
                .await
            {
                Ok(res) => {
                    let state = fetch_events(
                        &db,
                        server_name,
                        &pub_key_map,
                        &res.pdu_ids,
                        &mut auth_cache,
                    )
                    .await?;
                    // Sanity check: there are no conflicting events in the state we received
                    let mut seen = BTreeSet::new();
                    for ev in &state {
                        // If the key is already present
                        if !seen.insert((&ev.kind, &ev.state_key)) {
                            todo!("Server sent us an invalid state")
                        }
                    }

                    let state = state
                        .into_iter()
                        .map(|pdu| ((pdu.kind.clone(), pdu.state_key.clone()), pdu))
                        .collect();

                    (
                        state,
                        fetch_events(
                            &db,
                            server_name,
                            &pub_key_map,
                            &res.auth_chain_ids,
                            &mut auth_cache,
                        )
                        .await?,
                    )
                }
                Err(_) => {
                    resolved_map.insert(
                        pdu.event_id().clone(),
                        Err("Fetching state for event failed".into()),
                    );
                    continue;
                }
            };

        // 10. This is the actual auth check for state at the event
        if !state_res::event_auth::auth_check(
            &RoomVersionId::Version6,
            &pdu,
            single_prev.clone(),
            &state_at_event,
            None, // TODO: third party invite
        )
        .map_err(|_e| Error::Conflict("Auth check failed"))?
        {
            // Event failed auth with state_at
            resolved_map.insert(
                event_id,
                Err("Event has failed auth check with state at the event".into()),
            );
            continue;
        }
        // End of step 10.

        // 12. check if the event passes auth based on the "current state" of the room, if not "soft fail" it
        let current_state = db
            .rooms
            .room_state_full(pdu.room_id())?
            .into_iter()
            .map(|(k, v)| ((k.0, Some(k.1)), Arc::new(v)))
            .collect();

        if !state_res::event_auth::auth_check(
            &RoomVersionId::Version6,
            &pdu,
            single_prev.clone(),
            &current_state,
            None,
        )
        .map_err(|_e| Error::Conflict("Auth check failed"))?
        {
            // Soft fail, we add the event as an outlier.
            resolved_map.insert(
                pdu.event_id().clone(),
                Err("Event has been soft failed".into()),
            );
        };

        // Step 11. Ensure that the state is derived from the previous current state (i.e. we calculated by doing state res
        // where one of the inputs was a previously trusted set of state, don't just trust a set of state we got from a remote)
        //
        // calculate_forward_extremities takes care of adding the current state if not already in the state sets
        // it also calculates the new pdu leaves for the `roomid_pduleaves` DB Tree.
        let (mut fork_states, fork_ids) = match calculate_forward_extremities(
            &db,
            &pdu,
            server_name,
            &pub_key_map,
            &mut auth_cache,
        )
        .await
        {
            Ok(states) => states,
            Err(_) => {
                resolved_map.insert(event_id, Err("Failed to gather forward extremities".into()));
                continue;
            }
        };

        // add the incoming events to the mix of state snapshots
        // Since we are using a BTreeSet (yea this may be overkill) we guarantee unique state sets
        fork_states.insert(state_at_event.clone());

        let fork_states = fork_states.into_iter().collect::<Vec<_>>();

        // 13. start state-res with all previous forward extremities minus the ones that are in
        // the prev_events of this event plus the new one created by this event and use
        // the result as the new room state
        let state_at_forks = if fork_states.is_empty() {
            // State is empty
            Default::default()
        } else if fork_states.len() == 1 {
            fork_states[0].clone()
        } else {
            // TODO: remove this is for current debugging Jan, 15 2021
            let mut number_fetches = 0_u32;
            let mut auth_events = vec![];
            // this keeps track if we error so we can break out of these inner loops
            // to continue on with the incoming PDU's
            for map in &fork_states {
                let mut state_auth = vec![];
                for auth_id in map.values().flat_map(|pdu| &pdu.auth_events) {
                    let event = match auth_cache.get(auth_id) {
                        Some(aev) => aev.clone(),
                        // We should know about every event at this point but just incase...
                        None => match fetch_events(
                            &db,
                            server_name,
                            &pub_key_map,
                            &[auth_id.clone()],
                            &mut auth_cache,
                        )
                        .await
                        .map(|mut vec| {
                            number_fetches += 1;
                            vec.remove(0)
                        }) {
                            Ok(aev) => aev,
                            Err(_) => {
                                resolved_map.insert(
                                    event_id.clone(),
                                    Err("Event has been soft failed".into()),
                                );
                                continue 'main_pdu_loop;
                            }
                        },
                    };
                    state_auth.push(event);
                }
                auth_events.push(state_auth);
            }
            info!("{} event's were not in the auth_cache", number_fetches);

            let mut event_map = EventMap::new();
            // Add everything we will need to event_map
            event_map.extend(
                auth_events
                    .iter()
                    .map(|pdus| pdus.iter().map(|pdu| (pdu.event_id().clone(), pdu.clone())))
                    .flatten(),
            );
            event_map.extend(
                incoming_auth_events
                    .into_iter()
                    .map(|pdu| (pdu.event_id().clone(), pdu)),
            );
            event_map.extend(
                state_at_event
                    .into_iter()
                    .map(|(_, pdu)| (pdu.event_id().clone(), pdu)),
            );

            match state_res::StateResolution::resolve(
                &pdu.room_id,
                &RoomVersionId::Version6,
                &fork_states
                    .into_iter()
                    .map(|map| {
                        map.into_iter()
                            .map(|(k, v)| (k, v.event_id.clone()))
                            .collect::<StateMap<_>>()
                    })
                    .collect::<Vec<_>>(),
                auth_events
                    .into_iter()
                    .map(|pdus| pdus.into_iter().map(|pdu| pdu.event_id().clone()).collect())
                    .collect(),
                &mut event_map,
            ) {
                Ok(res) => res
                    .into_iter()
                    .map(|(k, v)| (k, Arc::new(db.rooms.get_pdu(&v).unwrap().unwrap())))
                    .collect(),
                Err(_) => {
                    resolved_map.insert(
                        pdu.event_id().clone(),
                        Err("State resolution failed, either an event could not be found or deserialization".into()),
                    );
                    continue 'main_pdu_loop;
                }
            }
        };

        if !state_res::event_auth::auth_check(
            &RoomVersionId::Version6,
            &pdu,
            single_prev,
            &state_at_forks,
            None,
        )
        .map_err(|_e| Error::Conflict("Auth check failed"))?
        {
            // Soft fail, we add the event as an outlier.
            resolved_map.insert(
                pdu.event_id().clone(),
                Err("Event has been soft failed".into()),
            );
        } else {
            // Add the event to the DB and update the forward extremities (via roomid_pduleaves).
            append_state(&db, &pdu, &fork_ids)?;

            // Event has passed all auth/stateres checks
            resolved_map.insert(pdu.event_id().clone(), Ok(()));
        }
    }

    Ok(dbg!(send_transaction_message::v1::Response { pdus: resolved_map }).into())
}

/// An async function that can recursively calls itself.
type AsyncRecursiveResult<'a, T> = Pin<Box<dyn Future<Output = StdResult<T, String>> + 'a + Send>>;

/// TODO: don't add as outlier if event is fetched as a result of gathering auth_events
/// Validate any event that is given to us by another server.
///
/// 1. Is a valid event, otherwise it is dropped (PduEvent deserialization satisfies this).
/// 2. check content hash, redact if doesn't match
/// 3. fetch any missing auth events doing all checks listed here starting at 1. These are not timeline events
/// 4. reject "due to auth events" if can't get all the auth events or some of the auth events are also rejected "due to auth events"
/// 5. reject "due to auth events" if the event doesn't pass auth based on the auth events
/// 7. if not timeline event: stop
/// 8. fetch any missing prev events doing all checks listed here starting at 1. These are timeline events
fn validate_event<'a>(
    db: &'a Database,
    value: CanonicalJsonObject,
    event_id: EventId,
    pub_key_map: &'a PublicKeyMap,
    origin: &'a ServerName,
    auth_cache: &'a mut EventMap<Arc<PduEvent>>,
) -> AsyncRecursiveResult<'a, (Arc<PduEvent>, Vec<Arc<PduEvent>>)> {
    Box::pin(async move {
        let mut val =
            match ruma::signatures::verify_event(pub_key_map, &value, &RoomVersionId::Version6) {
                Ok(ver) => {
                    if let ruma::signatures::Verified::Signatures = ver {
                        match ruma::signatures::redact(&value, &RoomVersionId::Version6) {
                            Ok(obj) => obj,
                            Err(_) => return Err("Redaction failed".to_string()),
                        }
                    } else {
                        value
                    }
                }
                Err(_e) => {
                    return Err("Signature verification failed".to_string());
                }
            };

        // Now that we have checked the signature and hashes we can add the eventID and convert
        // to our PduEvent type also finally verifying the first step listed above
        val.insert(
            "event_id".to_owned(),
            to_canonical_value(&event_id).expect("EventId is a valid CanonicalJsonValue"),
        );
        let pdu = serde_json::from_value::<PduEvent>(
            serde_json::to_value(val).expect("CanonicalJsonObj is a valid JsonValue"),
        )
        .map_err(|_| "Event is not a valid PDU".to_string())?;

        fetch_check_auth_events(db, origin, pub_key_map, &pdu.auth_events, auth_cache)
            .await
            .map_err(|_| "Event failed auth chain check".to_string())?;

        let pdu = Arc::new(pdu.clone());

        // 8. fetch any missing prev events doing all checks listed here starting at 1. These are timeline events
        let previous = fetch_events(&db, origin, &pub_key_map, &pdu.prev_events, auth_cache)
            .await
            .map_err(|e| e.to_string())?;

        // Check that the event passes auth based on the auth_events
        let is_authed = state_res::event_auth::auth_check(
            &RoomVersionId::Version6,
            &pdu,
            if previous.len() == 1 {
                previous.first().cloned()
            } else {
                None
            },
            &pdu.auth_events
                .iter()
                .map(|id| {
                    auth_cache
                        .get(id)
                        .map(|pdu| ((pdu.kind(), pdu.state_key()), pdu.clone()))
                        .ok_or_else(|| {
                            "Auth event not found, event failed recursive auth checks.".to_string()
                        })
                })
                .collect::<StdResult<BTreeMap<_, _>, _>>()?,
            None, // TODO: third party invite
        )
        .map_err(|_e| "Auth check failed".to_string())?;

        if !is_authed {
            return Err("Event has failed auth check with auth events".to_string());
        }

        Ok((pdu, previous))
    })
}

/// TODO: don't add as outlier if event is fetched as a result of gathering auth_events
/// The check in `fetch_check_auth_events` is that a complete chain is found for the
/// events `auth_events`. If the chain is found to have any missing events it fails.
///
/// The `auth_cache` is filled instead of returning a `Vec`.
async fn fetch_check_auth_events(
    db: &Database,
    origin: &ServerName,
    key_map: &PublicKeyMap,
    event_ids: &[EventId],
    auth_cache: &mut EventMap<Arc<PduEvent>>,
) -> Result<()> {
    let mut stack = event_ids.to_vec();

    // DFS for auth event chain
    while !stack.is_empty() {
        let ev_id = stack.pop().unwrap();
        if auth_cache.contains_key(&ev_id) {
            continue;
        }

        let ev = fetch_events(db, origin, key_map, &[ev_id.clone()], auth_cache)
            .await
            .map(|mut vec| {
                vec.pop()
                    .ok_or_else(|| Error::Conflict("Event was not found in fetch_events"))
            })??;

        stack.extend(ev.auth_events());
        auth_cache.insert(ev.event_id().clone(), ev);
    }
    Ok(())
}

/// Find the event and auth it.
///
/// 1. Look in the main timeline (pduid_pdu tree)
/// 2. Look at outlier pdu tree
/// 3. Ask origin server over federation
/// 4. TODO: Ask other servers over federation?
async fn fetch_events(
    db: &Database,
    origin: &ServerName,
    key_map: &PublicKeyMap,
    events: &[EventId],
    auth_cache: &mut EventMap<Arc<PduEvent>>,
) -> Result<Vec<Arc<PduEvent>>> {
    let mut pdus = vec![];
    for id in events {
        let pdu = match db.rooms.get_pdu(&id)? {
            Some(pdu) => Arc::new(pdu),
            None => match db.rooms.get_pdu_outlier(&id)? {
                Some(pdu) => Arc::new(pdu),
                None => match db
                    .sending
                    .send_federation_request(
                        &db.globals,
                        origin,
                        get_event::v1::Request { event_id: &id },
                    )
                    .await
                {
                    Ok(res) => {
                        let (event_id, value) = crate::pdu::gen_event_id_canonical_json(&res.pdu);
                        let (pdu, _) =
                            validate_event(db, value, event_id, key_map, origin, auth_cache)
                                .await
                                .map_err(|_| Error::Conflict("Authentication of event failed"))?;

                        pdu
                    }
                    Err(_) => return Err(Error::BadServerResponse("Failed to fetch event")),
                },
            },
        };
        pdus.push(pdu);
    }
    Ok(pdus)
}

/// Search the DB for the signing keys of the given server, if we don't have them
/// fetch them from the server and save to our DB.
async fn fetch_signing_keys(
    db: &Database,
    origin: &ServerName,
) -> Result<BTreeMap<ServerSigningKeyId, VerifyKey>> {
    match db.globals.signing_keys_for(origin)? {
        keys if !keys.is_empty() => Ok(keys),
        _ => {
            let keys = db
                .sending
                .send_federation_request(&db.globals, origin, get_server_keys::v2::Request::new())
                .await
                .map_err(|_| Error::BadServerResponse("Failed to request server keys"))?;
            db.globals.add_signing_key(origin, &keys.server_key)?;
            Ok(keys.server_key.verify_keys)
        }
    }
}

/// Gather all state snapshots needed to resolve the current state of the room.
///
/// Step 11. ensure that the state is derived from the previous current state (i.e. we calculated by doing state res
/// where one of the inputs was a previously trusted set of state, don't just trust a set of state we got from a remote).
///
/// The state snapshot of the incoming event __needs__ to be added to the resulting list.
async fn calculate_forward_extremities(
    db: &Database,
    pdu: &PduEvent,
    origin: &ServerName,
    pub_key_map: &PublicKeyMap,
    auth_cache: &mut EventMap<Arc<PduEvent>>,
) -> Result<(BTreeSet<StateMap<Arc<PduEvent>>>, Vec<EventId>)> {
    let mut current_leaves = db.rooms.get_pdu_leaves(pdu.room_id())?;

    let mut is_incoming_leaf = true;
    // Make sure the incoming event is not already a forward extremity
    // FIXME: I think this could happen if different servers send us the same event??
    if current_leaves.contains(pdu.event_id()) {
        is_incoming_leaf = false;
        // Not sure what to do here
    }

    // If the incoming event is already referenced by an existing event
    // then do nothing - it's not a candidate to be a new extremity if
    // it has been referenced.
    if already_referenced(db, pdu)? {
        is_incoming_leaf = false;
        // This event has been dealt with already??
    }

    // TODO:
    // [dendrite] Checks if any other leaves have been referenced and removes them
    // but as long as we update the pdu leaves here and for events on our server this
    // should not be possible.

    // Remove any forward extremities that are referenced by this incoming events prev_events
    for incoming_leaf in &pdu.prev_events {
        if current_leaves.contains(incoming_leaf) {
            if let Some(pos) = current_leaves.iter().position(|x| *x == *incoming_leaf) {
                current_leaves.remove(pos);
            }
        }
    }

    let current_hash = db.rooms.current_state_hash(pdu.room_id())?;

    let mut includes_current_state = false;
    let mut fork_states = BTreeSet::new();
    for id in &current_leaves {
        if let Some(id) = db.rooms.get_pdu_id(id)? {
            let state_hash = db
                .rooms
                .pdu_state_hash(&id)?
                .expect("found pdu with no statehash");

            if current_hash.as_ref() == Some(&state_hash) {
                includes_current_state = true;
            }
            let state = db
                .rooms
                .state_full(&pdu.room_id, &state_hash)?
                .into_iter()
                .map(|(k, v)| ((k.0, Some(k.1)), Arc::new(v)))
                .collect();

            fork_states.insert(state);
        } else {
            error!("Forward extremity not found... {}", id);

            let res = db
                .sending
                .send_federation_request(
                    &db.globals,
                    origin,
                    get_room_state_ids::v1::Request {
                        room_id: pdu.room_id(),
                        event_id: id,
                    },
                )
                .await?;

            // TODO: This only adds events to the auth_cache, there is for sure a better way to
            // do this...
            fetch_events(&db, origin, &pub_key_map, &res.auth_chain_ids, auth_cache).await?;

            let state = fetch_events(&db, origin, &pub_key_map, &res.pdu_ids, auth_cache)
                .await?
                .into_iter()
                .map(|pdu| ((pdu.kind.clone(), pdu.state_key.clone()), pdu))
                .collect();

            fork_states.insert(state);
        }
    }

    // Add the incoming event only if it is a leaf, we do this after fetching all the
    // state since we know we have already fetched the state of the incoming event so lets
    // not do it again!
    if is_incoming_leaf {
        current_leaves.push(pdu.event_id().clone());
    }

    // This guarantees that our current room state is included
    if !includes_current_state && current_hash.is_some() {
        fork_states.insert(
            db.rooms
                .state_full(pdu.room_id(), current_hash.as_ref().unwrap())?
                .into_iter()
                .map(|(k, v)| ((k.0, Some(k.1)), Arc::new(v)))
                .collect(),
        );
    }

    Ok((fork_states, dbg!(current_leaves)))
}

/// TODO: we need to know if the event is a prev_event (is this event already referenced in the DAG)
fn already_referenced(_db: &Database, _pdu: &PduEvent) -> Result<bool> {
    Ok(false)
}

fn append_state(db: &Database, pdu: &PduEvent, new_room_leaves: &[EventId]) -> Result<()> {
    let count = db.globals.next_count()?;
    let mut pdu_id = pdu.room_id.as_bytes().to_vec();
    pdu_id.push(0xff);
    pdu_id.extend_from_slice(&count.to_be_bytes());

    // We append to state before appending the pdu, so we don't have a moment in time with the
    // pdu without it's state. This is okay because append_pdu can't fail.
    let statehashid = db.rooms.append_to_state(&pdu_id, &pdu, &db.globals)?;

    db.rooms.append_pdu(
        pdu,
        utils::to_canonical_object(pdu).expect("Pdu is valid canonical object"),
        count,
        pdu_id.clone().into(),
        &db,
    )?;

    // If we update the room leaves after calling append_pdu it will stick since append_pdu
    // calls replace_pdu_leaves with only the given event.
    db.rooms.force_pdu_leaves(pdu.room_id(), new_room_leaves)?;

    // We set the room state after inserting the pdu, so that we never have a moment in time
    // where events in the current room state do not exist
    db.rooms.set_room_state(&pdu.room_id, &statehashid)?;

    for appservice in db.appservice.iter_all().filter_map(|r| r.ok()) {
        db.sending.send_pdu_appservice(&appservice.0, &pdu_id)?;
    }

    Ok(())
}

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/federation/v1/get_missing_events/<_>", data = "<body>")
)]
pub fn get_missing_events_route<'a>(
    db: State<'a, Database>,
    body: Ruma<get_missing_events::v1::Request<'_>>,
) -> ConduitResult<get_missing_events::v1::Response> {
    if !db.globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    let mut queued_events = body.latest_events.clone();
    let mut events = Vec::new();

    let mut i = 0;
    while i < queued_events.len() && events.len() < u64::from(body.limit) as usize {
        if let Some(pdu) = db.rooms.get_pdu_json(&queued_events[i])? {
            if body.earliest_events.contains(
                &serde_json::from_value(
                    pdu.get("event_id")
                        .cloned()
                        .ok_or_else(|| Error::bad_database("Event in db has no event_id field."))?,
                )
                .map_err(|_| Error::bad_database("Invalid event_id field in pdu in db."))?,
            ) {
                i += 1;
                continue;
            }
            queued_events.extend_from_slice(
                &serde_json::from_value::<Vec<EventId>>(
                    pdu.get("prev_events").cloned().ok_or_else(|| {
                        Error::bad_database("Invalid prev_events field of pdu in db.")
                    })?,
                )
                .map_err(|_| Error::bad_database("Invalid prev_events content in pdu in db."))?,
            );
            events.push(serde_json::from_value(pdu).expect("Raw<..> is always valid"));
        }
        i += 1;
    }

    Ok(get_missing_events::v1::Response { events }.into())
}

#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/federation/v1/query/profile", data = "<body>")
)]
pub fn get_profile_information_route<'a>(
    db: State<'a, Database>,
    body: Ruma<get_profile_information::v1::Request<'_>>,
) -> ConduitResult<get_profile_information::v1::Response> {
    if !db.globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    let mut displayname = None;
    let mut avatar_url = None;

    match &body.field {
        // TODO: what to do with custom
        Some(ProfileField::_Custom(_s)) => {}
        Some(ProfileField::DisplayName) => displayname = db.users.displayname(&body.user_id)?,
        Some(ProfileField::AvatarUrl) => avatar_url = db.users.avatar_url(&body.user_id)?,
        None => {
            displayname = db.users.displayname(&body.user_id)?;
            avatar_url = db.users.avatar_url(&body.user_id)?;
        }
    }

    Ok(get_profile_information::v1::Response {
        displayname,
        avatar_url,
    }
    .into())
}

/*
#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/federation/v2/invite/<_>/<_>", data = "<body>")
)]
pub fn get_user_devices_route<'a>(
    db: State<'a, Database>,
    body: Ruma<membership::v1::Request<'_>>,
) -> ConduitResult<get_profile_information::v1::Response> {
    if !db.globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    let mut displayname = None;
    let mut avatar_url = None;

    match body.field {
        Some(ProfileField::DisplayName) => displayname = db.users.displayname(&body.user_id)?,
        Some(ProfileField::AvatarUrl) => avatar_url = db.users.avatar_url(&body.user_id)?,
        None => {
            displayname = db.users.displayname(&body.user_id)?;
            avatar_url = db.users.avatar_url(&body.user_id)?;
        }
    }

    Ok(get_profile_information::v1::Response {
        displayname,
        avatar_url,
    }
    .into())
}
*/

#[cfg(test)]
mod tests {
    use super::{add_port_to_hostname, get_ip_with_port};

    #[test]
    fn ips_get_default_ports() {
        assert_eq!(
            get_ip_with_port(String::from("1.1.1.1")),
            Some(String::from("1.1.1.1:8448"))
        );
        assert_eq!(
            get_ip_with_port(String::from("dead:beef::")),
            Some(String::from("[dead:beef::]:8448"))
        );
    }

    #[test]
    fn ips_keep_custom_ports() {
        assert_eq!(
            get_ip_with_port(String::from("1.1.1.1:1234")),
            Some(String::from("1.1.1.1:1234"))
        );
        assert_eq!(
            get_ip_with_port(String::from("[dead::beef]:8933")),
            Some(String::from("[dead::beef]:8933"))
        );
    }

    #[test]
    fn hostnames_get_default_ports() {
        assert_eq!(
            add_port_to_hostname(String::from("example.com")),
            "example.com:8448"
        )
    }

    #[test]
    fn hostnames_keep_custom_ports() {
        assert_eq!(
            add_port_to_hostname(String::from("example.com:1337")),
            "example.com:1337"
        )
    }
}
