use std::convert::TryInto;

use anyhow::Result;
use sha2::Digest;
use ton_api::ton::TLObject;
use ton_api::{BoxedSerialize, Deserializer, IntoBoxed, Serializer};

pub use self::address_list::*;
pub use self::handshake::*;
pub use self::node_id::*;
pub use self::packet_view::*;
pub use self::queries_cache::*;
pub use self::query::*;

mod address_list;
mod handshake;
mod node_id;
mod packet_view;
mod queries_cache;
mod query;

pub fn hash<T: IntoBoxed>(object: T) -> Result<[u8; 32]> {
    hash_boxed(&object.into_boxed())
}

pub fn gen_packet_offset() -> Vec<u8> {
    use rand::Rng;

    const RAND_SIZE: usize = 16; // TODO: randomly choose between 7 and 15

    let mut result = vec![0; RAND_SIZE];
    rand::thread_rng().fill(result.as_mut_slice());
    result
}

pub fn build_packet_cipher(shared_secret: &[u8; 32], checksum: &[u8; 32]) -> aes::Aes256Ctr {
    use aes::cipher::NewCipher;

    let mut aes_key_bytes: [u8; 32] = *shared_secret;
    aes_key_bytes[16..32].copy_from_slice(&checksum[16..32]);
    let mut aes_ctr_bytes: [u8; 16] = checksum[0..16].try_into().unwrap();
    aes_ctr_bytes[4..16].copy_from_slice(&shared_secret[20..32]);

    aes::Aes256Ctr::new(
        generic_array::GenericArray::from_slice(&aes_key_bytes),
        generic_array::GenericArray::from_slice(&aes_ctr_bytes),
    )
}

pub fn compute_shared_secret(
    private_key_part: &[u8; 32],
    public_key: &[u8; 32],
) -> Result<[u8; 32]> {
    let point = curve25519_dalek::edwards::CompressedEdwardsY(*public_key)
        .decompress()
        .ok_or(BadPublicKeyData)?
        .to_montgomery()
        .to_bytes();
    Ok(x25519_dalek::x25519(*private_key_part, point))
}

#[derive(thiserror::Error, Debug)]
#[error("Bad public key data")]
struct BadPublicKeyData;

/// Calculates hash of TL object
pub fn hash_boxed<T: BoxedSerialize>(object: &T) -> Result<[u8; 32]> {
    let buf = sha2::Sha256::digest(&serialize(object)?);
    Ok(buf.as_slice().try_into().unwrap())
}

pub fn serialize<T: BoxedSerialize>(object: &T) -> Result<Vec<u8>> {
    let mut ret = Vec::new();
    Serializer::new(&mut ret).write_boxed(object).convert()?;
    Ok(ret)
}

pub fn serialize_boxed<T: IntoBoxed>(object: T) -> Result<Vec<u8>> {
    let object = object.into_boxed();
    serialize(&object)
}

pub fn serialize_append<T>(buffer: &mut Vec<u8>, object: &T) -> Result<()>
where
    T: BoxedSerialize,
{
    Serializer::new(buffer).write_boxed(object).convert()
}

pub fn serialize_inplace<T>(buffer: &mut Vec<u8>, object: &T) -> Result<()>
where
    T: BoxedSerialize,
{
    buffer.truncate(0);
    serialize_append(buffer, object)
}

/// Deserializes TL object from bytes
pub fn deserialize(bytes: &[u8]) -> Result<TLObject> {
    let mut reader = bytes;
    Deserializer::new(&mut reader)
        .read_boxed::<TLObject>()
        .convert()
}

/// Deserializes a bundle of TL objects from bytes
pub fn deserialize_bundle(mut bytes: &[u8]) -> Result<Vec<TLObject>> {
    let mut deserializer = Deserializer::new(&mut bytes);
    let mut result = Vec::new();
    loop {
        match deserializer.read_boxed::<TLObject>() {
            Ok(object) => result.push(object),
            Err(error) => {
                if result.is_empty() {
                    return Err(error).convert();
                } else {
                    break;
                }
            }
        }
    }
    Ok(result)
}

pub fn now() -> i32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i32
}

pub trait NoFailure {
    type Output;

    fn convert(self) -> anyhow::Result<Self::Output>;
}

impl<T> NoFailure for ton_types::Result<T> {
    type Output = T;

    fn convert(self) -> anyhow::Result<Self::Output> {
        self.map_err(|e| anyhow::Error::msg(e.to_string()))
    }
}
