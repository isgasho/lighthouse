use crate::Error;
use ssz::{Decode, Encode};
use std::marker::PhantomData;
use tree_hash::TreeHash;

pub const SIGNATURE_BYTES_LEN: usize = 96;

pub trait TSignature<PublicKey>: Sized {
    fn zero() -> Self;

    fn add_assign(&mut self, other: &Self);

    fn serialize(&self) -> [u8; SIGNATURE_BYTES_LEN];

    fn deserialize(bytes: &[u8]) -> Result<Self, Error>;

    fn verify(&self, pubkey: &PublicKey, msg: &[u8]) -> bool;

    fn fast_aggregate_verify(&self, pubkeys: &[PublicKey], msg: &[u8]) -> bool;
}

pub struct Signature<PublicKey, T: TSignature<PublicKey>> {
    point: T,
    _phantom: PhantomData<PublicKey>,
}

impl<PublicKey, T: TSignature<PublicKey>> Signature<PublicKey, T> {
    pub fn zero() -> Self {
        Self {
            point: T::zero(),
            _phantom: PhantomData,
        }
    }

    pub fn add_assign(&mut self, other: &Self) {
        self.point.add_assign(&other.point)
    }

    pub fn serialize(&self) -> [u8; SIGNATURE_BYTES_LEN] {
        self.point.serialize()
    }

    pub fn deserialize(bytes: &[u8]) -> Result<Self, Error> {
        Ok(Self {
            point: T::deserialize(bytes)?,
            _phantom: PhantomData,
        })
    }

    pub fn verify(&self, pubkey: &PublicKey, msg: &[u8]) -> bool {
        self.verify(pubkey, msg)
    }

    pub fn fast_aggregate_verify(&self, pubkeys: &[PublicKey], msg: &[u8]) -> bool {
        self.fast_aggregate_verify(pubkeys, msg)
    }
}

impl<PublicKey, T: TSignature<PublicKey>> Encode for Signature<PublicKey, T> {
    impl_ssz_encode!(SIGNATURE_BYTES_LEN);
}

impl<PublicKey, T: TSignature<PublicKey>> Decode for Signature<PublicKey, T> {
    impl_ssz_decode!(SIGNATURE_BYTES_LEN);
}

impl<PublicKey, T: TSignature<PublicKey>> TreeHash for Signature<PublicKey, T> {
    impl_tree_hash!(SIGNATURE_BYTES_LEN);
}