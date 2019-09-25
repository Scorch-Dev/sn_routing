// Copyright 2019 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

#[cfg(feature = "mock_parsec")]
use crate::mock::parsec as inner;
use crate::{
    chain::{self, GenesisPfxInfo},
    id::{self, FullId},
    messages::DirectMessage,
    utils::LogIdent,
};
use log::LogLevel;
use maidsafe_utilities::serialisation;
#[cfg(not(feature = "mock_parsec"))]
use parsec as inner;
use rand::Rng;
use std::{
    collections::{btree_map::Entry, BTreeMap},
    fmt,
};

#[cfg(feature = "mock_parsec")]
pub use crate::mock::parsec::{
    init_mock, ConsensusMode, Error, NetworkEvent, Observation, Proof, PublicId, SecretId,
};
#[cfg(not(feature = "mock_parsec"))]
pub use parsec::{ConsensusMode, Error, NetworkEvent, Observation, Proof, PublicId, SecretId};

pub type Block = inner::Block<chain::NetworkEvent, id::PublicId>;
pub type Parsec = inner::Parsec<chain::NetworkEvent, FullId>;
pub type Request = inner::Request<chain::NetworkEvent, id::PublicId>;
pub type Response = inner::Response<chain::NetworkEvent, id::PublicId>;

// TODO: we'll set PARSEC_SIZE_LIMIT to 1 GB once it's used outside of mock_parsec
//#[cfg(not(feature = "mock_parsec"))]
//const PARSEC_SIZE_LIMIT: u64 = 1_000_000_000;
// Mock parsec request/responses are much smaller, so we need a lower limit.
// TODO: once it's used outside of tests, this should be changed to cfg(feature = "mock_parsec")
#[cfg(all(test, feature = "mock_parsec"))]
const PARSEC_SIZE_LIMIT: u64 = 100;

// Keep track of size in case we need to prune.
#[derive(Default, Debug, PartialEq, Eq)]
struct ParsecSizeCounter(u64);

impl ParsecSizeCounter {
    fn increase_size(&mut self, size: u64) {
        self.0 += size;
    }

    #[cfg(all(test, feature = "mock_parsec"))]
    fn needs_pruning(&self) -> bool {
        self.0 > PARSEC_SIZE_LIMIT
    }
}

impl fmt::Display for ParsecSizeCounter {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "size: {}", self.0)
    }
}

pub struct ParsecMap {
    map: BTreeMap<u64, Parsec>,
    size_counter: ParsecSizeCounter,
}

impl ParsecMap {
    pub fn new(full_id: FullId, gen_pfx_info: &GenesisPfxInfo) -> Self {
        let mut map = BTreeMap::new();
        let _ = map.insert(
            *gen_pfx_info.first_info.version(),
            create(full_id, gen_pfx_info),
        );
        let size_counter = ParsecSizeCounter::default();

        Self { map, size_counter }
    }

    pub fn init(&mut self, full_id: FullId, gen_pfx_info: &GenesisPfxInfo, log_ident: &LogIdent) {
        if let Entry::Vacant(entry) = self.map.entry(*gen_pfx_info.first_info.version()) {
            let _ = entry.insert(create(full_id, gen_pfx_info));
            self.size_counter = ParsecSizeCounter::default();
            info!(
                "{}: Init new Parsec, genesis = {:?}",
                log_ident, gen_pfx_info
            );
        }
    }

    pub fn handle_request(
        &mut self,
        msg_version: u64,
        request: Request,
        pub_id: id::PublicId,
        log_ident: &LogIdent,
    ) -> (Option<DirectMessage>, bool) {
        // Increase the size before fetching the parsec to satisfy the borrow checker
        self.count_size(
            serialisation::serialised_size(&request),
            msg_version,
            log_ident,
        );

        let parsec = if let Some(parsec) = self.map.get_mut(&msg_version) {
            parsec
        } else {
            return (None, false);
        };

        let response = parsec
            .handle_request(&pub_id, request)
            .map(|response| DirectMessage::ParsecResponse(msg_version, response))
            .map_err(|err| {
                debug!("{} - Error handling parsec request: {:?}", log_ident, err);
                err
            })
            .ok();
        let poll = self.last_version() == msg_version;

        (response, poll)
    }

    pub fn handle_response(
        &mut self,
        msg_version: u64,
        response: Response,
        pub_id: id::PublicId,
        log_ident: &LogIdent,
    ) -> bool {
        // Increase the size before fetching the parsec to satisfy the borrow checker
        self.count_size(
            serialisation::serialised_size(&response),
            msg_version,
            log_ident,
        );

        let parsec = if let Some(parsec) = self.map.get_mut(&msg_version) {
            parsec
        } else {
            return false;
        };

        if let Err(err) = parsec.handle_response(&pub_id, response) {
            debug!("{} - Error handling parsec response: {:?}", log_ident, err);
        }

        self.last_version() == msg_version
    }

    pub fn create_gossip(&mut self, version: u64, target: &id::PublicId) -> Option<DirectMessage> {
        let request = self.map.get_mut(&version)?.create_gossip(target).ok()?;
        Some(DirectMessage::ParsecRequest(version, request))
    }

    pub fn vote_for(&mut self, event: chain::NetworkEvent, log_ident: &LogIdent) {
        if let Some(ref mut parsec) = self.map.values_mut().last() {
            let obs = match event.into_obs() {
                Err(_) => {
                    warn!(
                        "{} - Failed to convert NetworkEvent to Parsec Observation.",
                        log_ident
                    );
                    return;
                }
                Ok(obs) => obs,
            };

            if let Err(err) = parsec.vote_for(obs) {
                trace!("{} - Parsec vote error: {:?}", log_ident, err);
            }
        }
    }

    pub fn last_version(&self) -> u64 {
        if let Some(version) = self.map.keys().last() {
            *version
        } else {
            log_or_panic!(LogLevel::Error, "ParsecMap is empty.");
            0
        }
    }

    pub fn gossip_recipients(&self) -> Vec<&id::PublicId> {
        self.map
            .values()
            .last()
            .map(|parsec| parsec.gossip_recipients().collect())
            .unwrap_or_else(Vec::new)
    }

    pub fn poll(&mut self) -> Option<Block> {
        self.map.values_mut().last().and_then(Parsec::poll)
    }

    pub fn our_unpolled_observations(
        &self,
    ) -> impl Iterator<Item = &Observation<chain::NetworkEvent, id::PublicId>> {
        self.map
            .values()
            .last()
            .map(Parsec::our_unpolled_observations)
            .into_iter()
            .flatten()
    }

    #[cfg(feature = "mock_base")]
    pub fn has_unpolled_observations(&self) -> bool {
        let parsec = if let Some(parsec) = self.map.values().last() {
            parsec
        } else {
            return false;
        };

        parsec.has_unpolled_observations()
    }

    fn count_size(&mut self, size: u64, msg_version: u64, log_ident: &LogIdent) {
        if self.last_version() == msg_version && self.map.contains_key(&msg_version) {
            self.size_counter.increase_size(size);
            trace!(
                "{} - Parsec size is now estimated to: {}.",
                log_ident,
                self.size_counter,
            );
        }
    }

    #[cfg(all(test, feature = "mock_parsec"))]
    fn needs_pruning(&self) -> bool {
        self.size_counter.needs_pruning()
    }
}

/// Create Parsec instance.
fn create(full_id: FullId, gen_pfx_info: &GenesisPfxInfo) -> Parsec {
    let rng = new_rng();

    if gen_pfx_info
        .first_info
        .members()
        .contains(full_id.public_id())
    {
        Parsec::from_genesis(
            #[cfg(feature = "mock_parsec")]
            *gen_pfx_info.first_info.hash(),
            full_id,
            &gen_pfx_info.first_info.members(),
            gen_pfx_info.first_state_serialized.clone(),
            ConsensusMode::Single,
            rng,
        )
    } else {
        Parsec::from_existing(
            #[cfg(feature = "mock_parsec")]
            *gen_pfx_info.first_info.hash(),
            full_id,
            &gen_pfx_info.first_info.members(),
            &gen_pfx_info.latest_info.members(),
            ConsensusMode::Single,
            rng,
        )
    }
}

// Create new Rng instance.
//
// In production, use `OsRng` for maximum cryptographic security.
//
// In test, use a weaker rng to prevent creating too many open file handles (as each `OsRng`
// instance might internally contain a handle to /dev/random or similar). This avoid a bug with too
// many open files which sometimes happens because some test create lot of nodes and each node has
// its own rng.
fn new_rng() -> Box<dyn Rng> {
    if cfg!(feature = "mock_base") {
        use maidsafe_utilities::SeededRng;
        Box::new(SeededRng::thread_rng())
    } else {
        use rand::os::OsRng;
        let rng = match OsRng::new() {
            Ok(rng) => rng,
            Err(error) => panic!("Failed to create OsRng: {:?}", error),
        };

        Box::new(rng)
    }
}

#[cfg(all(test, feature = "mock_parsec"))]
mod tests {
    use super::*;
    use crate::{chain::SectionInfo, routing_table::Prefix, xor_name::XorName};
    use serde::Serialize;
    use unwrap::unwrap;

    const DEFAULT_MIN_SECTION_SIZE: usize = 4;

    #[test]
    fn parsec_size_counter() {
        let mut counter = ParsecSizeCounter::default();
        assert!(!counter.needs_pruning());
        counter.increase_size(PARSEC_SIZE_LIMIT);
        assert!(!counter.needs_pruning());
        counter.increase_size(1);
        assert!(counter.needs_pruning());
    }

    fn create_full_ids() -> Vec<FullId> {
        (0..DEFAULT_MIN_SECTION_SIZE)
            .map(|_| FullId::new())
            .collect()
    }

    fn create_gen_pfx_info(full_ids: Vec<FullId>, version: u64) -> GenesisPfxInfo {
        let members = full_ids.iter().map(|id| *id.public_id()).collect();
        let section_info = unwrap!(SectionInfo::new_for_test(
            members,
            Prefix::<XorName>::default(),
            version
        ));
        GenesisPfxInfo {
            first_info: section_info,
            first_state_serialized: Vec::new(),
            latest_info: SectionInfo::default(),
        }
    }

    fn create_parsec_map(size: u64) -> ParsecMap {
        let log_ident = LogIdent::new("node");
        let full_ids = create_full_ids();
        let full_id = full_ids[0].clone();

        let gen_pfx_info = create_gen_pfx_info(full_ids.clone(), 0);
        let mut parsec_map = ParsecMap::new(full_id.clone(), &gen_pfx_info);

        for parsec_no in 1..=size {
            let gen_pfx_info = create_gen_pfx_info(full_ids.clone(), parsec_no);
            parsec_map.init(full_id.clone(), &gen_pfx_info, &log_ident);
        }

        parsec_map
    }

    fn add_to_parsec_map(parsec_map: &mut ParsecMap, version: u64) {
        let log_ident = LogIdent::new("node");
        let full_ids = create_full_ids();
        let full_id = full_ids[0].clone();

        let gen_pfx_info = create_gen_pfx_info(full_ids, version);
        parsec_map.init(full_id, &gen_pfx_info, &log_ident);
    }

    trait HandleRequestResponse {
        fn handle(
            &self,
            parsec_map: &mut ParsecMap,
            msg_version: u64,
            pub_id: &id::PublicId,
            log_ident: &LogIdent,
        );
    }

    impl HandleRequestResponse for Request {
        fn handle(
            &self,
            parsec_map: &mut ParsecMap,
            msg_version: u64,
            pub_id: &id::PublicId,
            log_ident: &LogIdent,
        ) {
            let _ = parsec_map.handle_request(msg_version, self.clone(), *pub_id, &log_ident);
        }
    }

    impl HandleRequestResponse for Response {
        fn handle(
            &self,
            parsec_map: &mut ParsecMap,
            msg_version: u64,
            pub_id: &id::PublicId,
            log_ident: &LogIdent,
        ) {
            let _ = parsec_map.handle_response(msg_version, self.clone(), *pub_id, &log_ident);
        }
    }

    fn handle_msgs_just_below_prune_limit<T: HandleRequestResponse + Serialize>(
        parsec_map: &mut ParsecMap,
        msg_version: u64,
        msg: &T,
        pub_id: &id::PublicId,
        log_ident: &LogIdent,
    ) {
        let msg_size = serialisation::serialised_size(&msg);
        let msg_size_limit = PARSEC_SIZE_LIMIT / msg_size;

        // Handle msg_size_limit msgs which should trigger pruning needed on the next
        // msg if it's against the latest parsec.
        for _ in 0..msg_size_limit {
            msg.handle(parsec_map, msg_version, pub_id, log_ident);
        }

        // Make sure we don't cross the prune limit
        assert_eq!(parsec_map.needs_pruning(), false);
    }

    fn check_prune_needed_after_msg<T: HandleRequestResponse + Serialize>(
        msg: T,
        parsec_age: u64,
        prune_needed: bool,
    ) {
        let full_id = FullId::new();
        let pub_id = full_id.public_id();
        let number_of_parsecs = 2;

        let log_ident = LogIdent::new("node");
        let mut parsec_map = create_parsec_map(number_of_parsecs);

        // Sometimes send to an old parsec
        let msg_version = number_of_parsecs - parsec_age;

        handle_msgs_just_below_prune_limit(&mut parsec_map, msg_version, &msg, &pub_id, &log_ident);

        msg.handle(&mut parsec_map, msg_version, pub_id, &log_ident);
        assert_eq!(parsec_map.needs_pruning(), prune_needed);

        // Add another parsec should always reset `needs_pruning()`
        add_to_parsec_map(&mut parsec_map, number_of_parsecs + 1);
        assert_eq!(parsec_map.needs_pruning(), false);
    }

    #[test]
    fn prune_not_required_for_resp_to_old_parsec() {
        let parsec_age = 1;
        check_prune_needed_after_msg(Response::new(), parsec_age, false);
    }

    #[test]
    fn prune_required_for_resp_to_latest_parsec() {
        let parsec_age = 0;
        check_prune_needed_after_msg(Response::new(), parsec_age, true);
    }

    #[test]
    fn prune_not_required_for_req_to_old_parsec() {
        let parsec_age = 1;
        check_prune_needed_after_msg(Request::new(), parsec_age, false);
    }

    #[test]
    fn prune_required_for_req_to_latest_parsec() {
        let parsec_age = 0;
        check_prune_needed_after_msg(Request::new(), parsec_age, true);
    }
}
