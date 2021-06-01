// Copyright (c) Facebook, Inc. and its affiliates.
//
// This source code is licensed under the MIT license found in the
// LICENSE file in the root directory of this source tree.

use crate::{
    folding::apply_drp,
    utils::{fold_positions, hash_values},
    FriOptions, FriProof, FriProofLayer, ProverChannel,
};
use crypto::{Hasher, MerkleTree};
use math::field::{FieldElement, StarkField};
use std::marker::PhantomData;
use utils::{flatten_vector_elements, group_slice_elements, transpose_slice};

#[cfg(test)]
mod tests;

// TYPES AND INTERFACES
// ================================================================================================

pub struct FriProver<B, E, C, H>
where
    B: StarkField,
    E: FieldElement<BaseField = B>,
    C: ProverChannel<E, Hasher = H>,
    H: Hasher,
{
    options: FriOptions,
    layers: Vec<FriLayer<B, E, H>>,
    _coin: PhantomData<C>,
}

struct FriLayer<B: StarkField, E: FieldElement<BaseField = B>, H: Hasher> {
    tree: MerkleTree<H>,
    evaluations: Vec<E>,
    _base_field: PhantomData<B>,
}

// PROVER IMPLEMENTATION
// ================================================================================================

impl<B, E, C, H> FriProver<B, E, C, H>
where
    B: StarkField,
    E: FieldElement<BaseField = B>,
    C: ProverChannel<E, Hasher = H>,
    H: Hasher,
{
    // CONSTRUCTOR
    // --------------------------------------------------------------------------------------------
    /// Returns a new FRI prover instantiated with the provided options.
    pub fn new(options: FriOptions) -> Self {
        FriProver {
            options,
            layers: Vec::new(),
            _coin: PhantomData,
        }
    }

    // ACCESSORS
    // --------------------------------------------------------------------------------------------

    /// Returns folding factor for this prover.
    pub fn folding_factor(&self) -> usize {
        self.options.folding_factor()
    }

    /// Returns offset of the domain over which FRI protocol is executed by this prover.
    pub fn domain_offset(&self) -> B {
        self.options.domain_offset()
    }

    /// Returns number of FRI layers computed during the last execution of build_layers() method
    pub fn num_layers(&self) -> usize {
        self.layers.len()
    }

    /// Clears a vector of internally stored layers.
    pub fn reset(&mut self) {
        self.layers.clear();
    }

    // COMMIT PHASE
    // --------------------------------------------------------------------------------------------
    /// Executes commit phase of FRI protocol which recursively applies a degree-respecting
    /// projection to evaluations of some function F over a larger domain. The degree of the
    /// function implied by evaluations is reduced by folding_factor at every step until the
    /// remaining evaluations can fit into a vector of at most max_remainder_length. At each layer
    /// of recursion the current evaluations are committed to using a Merkle tree, and the root of
    /// this tree is used to derive randomness for the subsequent application of degree-respecting
    /// projection.
    pub fn build_layers(&mut self, channel: &mut C, mut evaluations: Vec<E>) {
        assert!(
            self.layers.is_empty(),
            "a prior proof generation request has not been completed yet"
        );

        // reduce the degree by folding_factor at each iteration until the remaining polynomial
        // is small enough; + 1 is for the remainder
        for _ in 0..self.options.num_fri_layers(evaluations.len()) + 1 {
            match self.folding_factor() {
                4 => self.build_layer::<4>(channel, &mut evaluations),
                8 => self.build_layer::<8>(channel, &mut evaluations),
                16 => self.build_layer::<16>(channel, &mut evaluations),
                _ => unimplemented!("folding factor {} is not supported", self.folding_factor()),
            }
        }

        // make sure remainder length does not exceed max allowed value
        let last_layer = &self.layers[self.layers.len() - 1];
        let remainder_size = last_layer.evaluations.len();
        debug_assert!(
            remainder_size <= self.options.max_remainder_size(),
            "last FRI layer cannot exceed {} elements, but was {} elements",
            self.options.max_remainder_size(),
            remainder_size
        );
    }

    /// Builds a single FRI layer by first committing to the `evaluations`, then drawing a random
    /// alpha from the channel and using it to perform degree-preserving projection.
    fn build_layer<const N: usize>(&mut self, channel: &mut C, evaluations: &mut Vec<E>) {
        // commit to the evaluations at the current layer; we do this by first transposing the
        // evaluations into a matrix of N columns, and then building a Merkle tree from the
        // rows of this matrix; we do this so that we could de-commit to N values with a single
        // Merkle authentication path.
        let transposed_evaluations = transpose_slice(evaluations);
        let hashed_evaluations = hash_values::<H, E, N>(&transposed_evaluations);
        let evaluation_tree = MerkleTree::<H>::new(hashed_evaluations);
        channel.commit_fri_layer(*evaluation_tree.root());

        // draw a pseudo-random coefficient from the channel, and use it in degree-respecting
        // projection to reduce the degree of evaluations by N
        let alpha = channel.draw_fri_alpha();
        *evaluations = apply_drp(&transposed_evaluations, self.domain_offset(), alpha);

        self.layers.push(FriLayer {
            tree: evaluation_tree,
            evaluations: flatten_vector_elements(transposed_evaluations),
            _base_field: PhantomData,
        });
    }

    // QUERY PHASE
    // --------------------------------------------------------------------------------------------
    /// Executes query phase of FRI protocol. For each of the provided `positions`, corresponding
    /// evaluations from each of the layers are recorded into the proof together with Merkle
    /// authentication paths from the root of layer commitment trees.
    pub fn build_proof(&mut self, positions: &[usize]) -> FriProof {
        assert!(
            !self.layers.is_empty(),
            "FRI layers have not been built yet"
        );
        let mut positions = positions.to_vec();
        let mut domain_size = self.layers[0].evaluations.len();
        let folding_factor = self.options.folding_factor();

        // for all FRI layers, except the last one, record tree root, determine a set of query
        // positions, and query the layer at these positions.
        let mut layers = Vec::with_capacity(self.layers.len());
        for i in 0..self.layers.len() - 1 {
            positions = fold_positions(&positions, domain_size, folding_factor);

            // sort of a static dispatch for folding_factor parameter
            let proof_layer = match folding_factor {
                4 => query_layer::<B, E, H, 4>(&self.layers[i], &positions),
                8 => query_layer::<B, E, H, 8>(&self.layers[i], &positions),
                16 => query_layer::<B, E, H, 16>(&self.layers[i], &positions),
                _ => unimplemented!("folding factor {} is not supported", folding_factor),
            };

            layers.push(proof_layer);
            domain_size /= folding_factor;
        }

        // use the remaining polynomial values directly as proof; last layer values contain
        // remainder in transposed form - so, we un-transpose it first
        let last_values = &self.layers[self.layers.len() - 1].evaluations;
        let mut remainder = E::zeroed_vector(last_values.len());
        let n = last_values.len() / folding_factor;
        for i in 0..n {
            for j in 0..folding_factor {
                remainder[i + n * j] = last_values[i * folding_factor + j];
            }
        }

        // clear layers so that another proof can be generated
        self.reset();

        FriProof::new(layers, remainder, 1)
    }
}

// HELPER FUNCTIONS
// ================================================================================================

/// Builds a single proof layer by querying the evaluations of the passed in FRI layer at the
/// specified positions.
fn query_layer<B: StarkField, E: FieldElement<BaseField = B>, H: Hasher, const N: usize>(
    layer: &FriLayer<B, E, H>,
    positions: &[usize],
) -> FriProofLayer {
    // build Merkle authentication paths for all query positions
    let proof = layer.tree.prove_batch(positions);

    // build a list of polynomial evaluations at each position; since evaluations in FRI layers
    // are stored in transposed form, a position refers to N evaluations which are committed
    // in a single leaf
    let mut queried_values: Vec<[E; N]> = Vec::with_capacity(positions.len());
    for &position in positions.iter() {
        let evaluations: &[[E; N]] = group_slice_elements(&layer.evaluations);
        queried_values.push(evaluations[position]);
    }

    FriProofLayer::new(queried_values, proof)
}
