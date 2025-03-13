// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::{generate_vec, make_rng, Generate};
use {fidl_next_test_benchmark as ftb_next, fidl_test_benchmark as ftb};

impl_generate! {
    for ftb::Vector3, ftb_next::Vector3 => rng {
        Self { x: rng.gen(), y: rng.gen(), z: rng.gen() }
    }
}

impl_generate! {
    for ftb::Triangle, ftb_next::Triangle => rng {
        Self {
            v0: Generate::generate(rng),
            v1: Generate::generate(rng),
            v2: Generate::generate(rng),
            normal: Generate::generate(rng),
        }
    }
}

pub fn generate_input_rust(input_size: usize) -> ftb::Mesh {
    let mut rng = make_rng();
    ftb::Mesh { triangles: generate_vec(&mut rng, input_size) }
}

pub fn generate_input_rust_next(input_size: usize) -> ftb_next::Mesh {
    let mut rng = make_rng();
    ftb_next::Mesh { triangles: generate_vec(&mut rng, input_size) }
}
