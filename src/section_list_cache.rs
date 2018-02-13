// Copyright 2016 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under (1) the MaidSafe.net Commercial License,
// version 1.0 or later, or (2) The General Public License (GPL), version 3, depending on which
// licence you accepted on initial access to the Software (the "Licences").
//
// By contributing code to the SAFE Network Software, or to this project generally, you agree to be
// bound by the terms of the MaidSafe Contributor Agreement.  This, along with the Licenses can be
// found in the root directory of this project at LICENSE, COPYING and CONTRIBUTOR.
//
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.
//
// Please review the Licences for the specific language governing permissions and limitations
// relating to use of the SAFE Network Software.

use super::{QUORUM_DENOMINATOR, QUORUM_NUMERATOR};
use super::XorName;
use itertools::Itertools;
use messages::SectionList;
use public_info::PublicInfo;
use routing_table::{Prefix, UnversionedPrefix};
use rust_sodium::crypto::sign::Signature;
use std::collections::HashMap;

pub type Signatures = HashMap<PublicInfo, Signature>;
pub type PrefixMap<T> = HashMap<UnversionedPrefix, T>;

#[derive(Default)]
pub struct SectionListCache {
    // all signatures for a section list for a given prefix
    signatures: PrefixMap<HashMap<SectionList, Signatures>>,
    // section lists signed by a given public id
    signed_by: HashMap<PublicInfo, PrefixMap<SectionList>>,
    // the latest section list for each prefix with a quorum of signatures
    lists_cache: PrefixMap<(SectionList, Signatures)>,
}

impl SectionListCache {
    pub fn new() -> SectionListCache {
        Default::default()
    }

    /// Removes all signatures authored by `name`
    pub fn remove_signatures(&mut self, name: &XorName, our_section_size: usize) {
        let pub_info_opt = self.signed_by
            .keys()
            .find(|pub_info| name == &pub_info.name())
            .cloned();
        if let Some(pub_info) = pub_info_opt {
            if let Some(lists) = self.signed_by.remove(&pub_info) {
                for (prefix, list) in lists {
                    let _ = self.signatures.get_mut(&prefix).and_then(|map| {
                        map.get_mut(&list).and_then(
                            |sigmap| sigmap.remove(&pub_info),
                        )
                    });
                }
                self.prune();
                self.update_lists_cache(our_section_size);
            }
        }
    }

    /// Adds a new signature for a section list
    pub fn add_signature(
        &mut self,
        prefix: Prefix,
        pub_info: PublicInfo,
        list: SectionList,
        sig: Signature,
        our_section_size: usize,
    ) {
        // remove all conflicting signatures
        self.remove_signatures_for_prefix_by(prefix, pub_info);
        // remember that this public id signed this section list
        let _ = self.signed_by
            .entry(pub_info)
            .or_insert_with(HashMap::new)
            .insert(prefix.unversioned(), list.clone());
        // remember that this section list has a new signature
        let _ = self.signatures
            .entry(prefix.unversioned())
            .or_insert_with(HashMap::new)
            .entry(list)
            .or_insert_with(HashMap::new)
            .insert(pub_info, sig);
        self.update_lists_cache(our_section_size);
    }

    /// Returns the given signature, if present.
    pub fn get_signature_for(
        &self,
        prefix: &Prefix,
        pub_info: &PublicInfo,
        list: &SectionList,
    ) -> Option<&Signature> {
        self.signatures
            .get(&prefix.unversioned())
            .and_then(|lists| lists.get(list))
            .and_then(|sigs| sigs.get(pub_info))
    }

    /// Returns the currently signed section list for `prefix` along with a quorum of signatures.
    // TODO: Remove this when the method is used in production
    #[cfg(feature = "use-mock-crust")]
    pub fn get_signatures(&self, prefix: &Prefix) -> Option<&(SectionList, Signatures)> {
        self.lists_cache.get(&prefix.unversioned())
    }

    fn prune(&mut self) {
        let mut to_remove = vec![];
        for (prefix, map) in &mut self.signatures {
            // prune section lists with 0 signatures
            let lists_to_remove = map.iter()
                .filter(|&(_, sigs)| sigs.is_empty())
                .map(|(list, _)| list.clone())
                .collect_vec();
            for list in lists_to_remove {
                let _ = map.remove(&list);
            }
            if map.is_empty() {
                to_remove.push(*prefix);
            }
        }
        // prune prefixes with no section lists
        for prefix in to_remove {
            let _ = self.signatures.remove(&prefix);
            // if we lose a prefix from `signatures`, there is no point in holding it in
            // `lists_cache`
            let _ = self.lists_cache.remove(&prefix);
        }

        let to_remove = self.signed_by
            .iter()
            .filter(|&(_, map)| map.is_empty())
            .map(|(pub_info, _)| *pub_info)
            .collect_vec();
        // prune pub_infos signing nothing
        for pub_info in to_remove {
            let _ = self.signed_by.remove(&pub_info);
        }
    }

    fn update_lists_cache(&mut self, our_section_size: usize) {
        for (prefix, map) in &self.signatures {
            // find the entries with the most signatures
            let entries = map.iter()
                .map(|(list, sigs)| (list, sigs.len()))
                .sorted_by(|lhs, rhs| rhs.1.cmp(&lhs.1));
            if let Some(&(list, sig_count)) = entries.first() {
                // entry.0 = list, entry.1 = num of signatures
                if sig_count * QUORUM_DENOMINATOR > our_section_size * QUORUM_NUMERATOR {
                    // we have a list with a quorum of signatures
                    let signatures = unwrap!(map.get(list));
                    let _ = self.lists_cache.insert(
                        *prefix,
                        (list.clone(), signatures.clone()),
                    );
                }
            }
        }
    }

    fn remove_signatures_for_prefix_by(&mut self, prefix: Prefix, author: PublicInfo) {
        // vector of tuples (prefix, section list) to be removed
        let to_remove = self.signed_by
            .get(&author)
            .into_iter()
            .flat_map(|map| map.iter())
            .filter(|&(p, _)| p.is_compatible(&prefix))
            .map(|(&prefix, list)| (prefix, list.clone()))
            .collect_vec();
        for (prefix, list) in to_remove {
            // remove the signatures from self.signatures
            let _ = self.signatures.get_mut(&prefix).and_then(|map| {
                map.get_mut(&list).and_then(|sigmap| sigmap.remove(&author))
            });
            // remove those entries from self.signed_by
            let _ = self.signed_by.get_mut(&author).and_then(
                |map| map.remove(&prefix),
            );
        }

        self.prune();
        // not updating the cache - removal of signatures shouldn't change it anyway, but even if
        // it does, this function is only called from `add_signature` and we update the cache there
    }
}
