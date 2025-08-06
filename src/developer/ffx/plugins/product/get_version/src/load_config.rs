// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::unique_release_info::{
    from_board_release_info, from_product_release_info, from_release_info, UniqueReleaseInfo,
};

use assembly_config_schema::{BoardConfig, BoardInputBundleSet, ProductConfig};
use assembly_partitions_config::Slot;
use assembly_platform_artifacts::PlatformArtifacts;
use assembly_release_info::SystemReleaseInfo;
use product_bundle::ProductBundleV2;
use product_input_bundle::ProductInputBundle;
use std::collections::BTreeMap;
use std::fmt;

/// VersionInfo holds the final content that will be printed out.
#[derive(Debug, PartialEq, Clone, Eq, serde::Serialize, serde::Deserialize, Default)]
pub struct VersionInfo {
    /// This will be printed if "--machine" is not given on the command-line.
    pub human: String,

    /// This will be printed if "--machine" is present on the command-line.
    pub machine: Vec<UniqueReleaseInfo>,
}

impl VersionInfo {
    /// Convert a VersionInfo instance into a VersionInfoWithDependencies
    /// by cloning itself.
    pub fn into_version_with_deps(self) -> VersionInfoWithDependencies {
        return VersionInfoWithDependencies { version: self.clone(), version_with_deps: self };
    }
}

/// VersionInfoWithDependencies is a collection containing the VersionInfo
/// for the target assembly artifact, and the version information for all
/// relevant dependencies.
pub struct VersionInfoWithDependencies {
    /// This will be printed by default.
    pub version: VersionInfo,

    /// This will be printed for product bunde artifacts
    /// if "--include-dependencies" is present on the command-line.
    pub version_with_deps: VersionInfo,
}

impl fmt::Display for VersionInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.human)
    }
}

impl PartialOrd for VersionInfo {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for VersionInfo {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.human.cmp(&other.human)
    }
}

/// Load a Platform artifact and return the version information.
pub fn load_platform(platform: &PlatformArtifacts) -> VersionInfo {
    VersionInfo {
        human: platform.release_info.version.clone(),
        machine: vec![from_release_info(&platform.release_info.clone(), &None)],
    }
}

/// Load a Product artifact and return the version information.
pub fn load_product(assembly_config: &ProductConfig) -> VersionInfo {
    VersionInfo {
        human: assembly_config.product.release_info.info.version.clone(),
        machine: vec![from_product_release_info(
            &assembly_config.product.release_info.clone(),
            &None,
        )],
    }
}

/// Load a Product Input Bundle artifact and return the version information.
pub fn load_pibs(pibs: &ProductInputBundle) -> VersionInfo {
    VersionInfo {
        human: pibs.release_info.version.clone(),
        machine: vec![from_release_info(&pibs.release_info.clone(), &None)],
    }
}

/// Load a Board Config artifact and return the version information.
pub fn load_board(board: &BoardConfig) -> VersionInfoWithDependencies {
    let mut info = VersionInfoWithDependencies {
        version: VersionInfo {
            human: board.release_info.info.version.clone(),
            machine: vec![from_board_release_info(&board.release_info.clone(), &None)],
        },
        version_with_deps: VersionInfo {
            human: board.release_info.info.version.clone(),
            machine: vec![from_board_release_info(&board.release_info.clone(), &None)],
        },
    };

    board.release_info.bib_sets.iter().for_each(|bib_set| {
        info.version_with_deps.human.push_str(&format!(
            "\n{}: {}",
            bib_set.name.clone(),
            bib_set.version.clone()
        ));

        let bib_set_info = from_release_info(bib_set, &None);
        info.version_with_deps.machine.push(bib_set_info);
    });

    info
}

/// Load a Board Input Bundle Set artifact and return the version information.
pub fn load_bib_set(bib_set: &BoardInputBundleSet) -> VersionInfo {
    VersionInfo {
        human: bib_set.release_info.version.clone(),
        machine: vec![from_release_info(&bib_set.release_info.clone(), &None)],
    }
}

/// Load a Product Bundle artifact and return the version information.
pub fn load_product_bundle_v2(pb: &ProductBundleV2) -> VersionInfoWithDependencies {
    let pb_info = pb.release_info.clone().unwrap();
    let mut btree: BTreeMap<UniqueReleaseInfo, Vec<Slot>> = BTreeMap::new();

    // If an existing UniqueReleaseInfo exists in the BTreeMap with the same
    // fields (excluding the Slot list), merge the two slot vectors into the
    // "value" for that entry in the BTreeMap.
    //
    // The Slot field in each UniqueReleaseInfo entry will be overwritten with
    // the contents of the BTreeMap values down below.
    let mut push_or_merge = |new_info: UniqueReleaseInfo| {
        let new_info_slot = new_info.slot.clone();
        btree
            .entry(new_info)
            .and_modify(|slot_vec| slot_vec.extend(&new_info_slot))
            .or_insert(new_info_slot);
    };

    let mut add_flat_system_info = |info: SystemReleaseInfo, slot: Slot| {
        push_or_merge(from_release_info(&info.platform, &Some(slot.clone())));

        let product = info.product;
        push_or_merge(from_release_info(&product.info, &Some(slot.clone())));
        product
            .pibs
            .iter()
            .for_each(|pib| push_or_merge(from_release_info(pib, &Some(slot.clone()))));

        let board = info.board;
        push_or_merge(from_release_info(&board.info, &Some(slot.clone())));
        board
            .bib_sets
            .iter()
            .for_each(|bib_set| push_or_merge(from_release_info(bib_set, &Some(slot.clone()))));
    };

    // Push release information for the systems inside the PB.
    pb_info.system_a.map(|system| add_flat_system_info(system, Slot::A));
    pb_info.system_b.map(|system| add_flat_system_info(system, Slot::B));
    pb_info.system_r.map(|system| add_flat_system_info(system, Slot::R));

    // Convert the btreemap to a vector of keys, and set each UniqueReleaseInfo
    // slot field equal to the corresponding "value" in the BTreeMap.
    let mut flat: Vec<UniqueReleaseInfo> = btree.clone().into_keys().collect();
    let mut flat_str = String::new();
    for info in flat.iter_mut() {
        info.slot = btree[info].clone();
        flat_str.push_str(&format!("\n{}: {}", info.name.clone(), info.version.clone()));
    }

    let pb_release_info = UniqueReleaseInfo {
        name: pb.product_name.clone(),
        version: pb.product_version.clone(),
        repository: "unspecified".to_string(), // Product Bundles do not have a repository.
        slot: vec![],
    };

    VersionInfoWithDependencies {
        version: VersionInfo { human: pb.product_version.clone(), machine: vec![pb_release_info] },
        version_with_deps: VersionInfo { human: flat_str, machine: flat },
    }
}
